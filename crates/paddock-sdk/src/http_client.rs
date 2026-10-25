//! HTTP requests from the app (`wasi:http/outgoing-handler`), blocking.
//!
//! The node sends a request only to a scheme, host and port the manifest
//! lists in `capabilities."wasi:http/outgoing-handler".allow`, and refuses
//! the rest with [`HttpError::Denied`]. Waiting for the response does not
//! count against the callback deadline, but the instance handles no other
//! message meanwhile; a pool of instances keeps an app responsive.
//!
//! ```ignore
//! use paddock_sdk::http_client;
//!
//! let reply = http_client::get("https://api.example.com/v1/status")?;
//! if reply.status == 200 {
//!     let text = String::from_utf8_lossy(&reply.body);
//! }
//! ```

use std::time::Duration;

use wasip2::http::outgoing_handler;
use wasip2::http::types::{
    ErrorCode, Fields, IncomingBody, Method, OutgoingBody, OutgoingRequest, RequestOptions, Scheme,
};
use wasip2::io::streams::StreamError;

/// A request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// `GET`, `POST`, ... (upper case).
    pub method: String,
    /// `http://` or `https://`, host, optional port, path and query.
    pub url: String,
    /// Header names and values.
    pub headers: Vec<(String, Vec<u8>)>,
    /// The body; empty for none.
    pub body: Vec<u8>,
    /// Most time to the response headers, and between chunks of its body;
    /// the node's own limits apply when shorter.
    pub timeout: Option<Duration>,
}

impl Request {
    /// A request with `method` to `url`, no headers and no body.
    #[must_use]
    pub fn new(method: &str, url: &str) -> Self {
        Self {
            method: method.to_string(),
            url: url.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: None,
        }
    }

    /// With a header.
    #[must_use]
    pub fn header(mut self, name: &str, value: impl Into<Vec<u8>>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    /// With a body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// With a timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// A response, read whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// Header names (lower case) and values.
    pub headers: Vec<(String, Vec<u8>)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Response {
    /// The first value of header `name`, as text.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
    }
}

/// Why a request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HttpError {
    /// Not allowed: the capability is not granted, or the scheme, host,
    /// port or method is not in the manifest's lists.
    Denied,
    /// The app already has `max_concurrent` requests in flight.
    Limited,
    /// The host name resolves only to loopback or link-local addresses,
    /// which the node refuses.
    Prohibited,
    /// No response, or a body chunk, within the timeout.
    Timeout,
    /// The host could not be found or reached.
    Unreachable(String),
    /// TLS failed (an untrusted certificate, a protocol error).
    Tls(String),
    /// The URL or a header is not valid.
    Invalid(String),
    /// Anything else, as the host reported it.
    Other(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied => f.write_str("denied"),
            Self::Limited => f.write_str("limited"),
            Self::Prohibited => f.write_str("prohibited"),
            Self::Timeout => f.write_str("timeout"),
            Self::Unreachable(e) => write!(f, "unreachable: {e}"),
            Self::Tls(e) => write!(f, "tls: {e}"),
            Self::Invalid(e) => write!(f, "invalid: {e}"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HttpError {}

impl From<ErrorCode> for HttpError {
    fn from(e: ErrorCode) -> Self {
        match e {
            ErrorCode::HttpRequestDenied => Self::Denied,
            ErrorCode::ConnectionLimitReached => Self::Limited,
            ErrorCode::DestinationIpProhibited => Self::Prohibited,
            ErrorCode::ConnectionReadTimeout
            | ErrorCode::ConnectionTimeout
            | ErrorCode::ConnectionWriteTimeout
            | ErrorCode::HttpResponseTimeout => Self::Timeout,
            ErrorCode::ConnectionRefused
            | ErrorCode::DestinationNotFound
            | ErrorCode::DestinationUnavailable
            | ErrorCode::DestinationIpUnroutable
            | ErrorCode::DnsTimeout
            | ErrorCode::DnsError(_)
            | ErrorCode::ConnectionTerminated => Self::Unreachable(format!("{e:?}")),
            ErrorCode::TlsProtocolError
            | ErrorCode::TlsCertificateError
            | ErrorCode::TlsAlertReceived(_) => Self::Tls(format!("{e:?}")),
            ErrorCode::HttpRequestMethodInvalid | ErrorCode::HttpRequestUriInvalid => {
                Self::Invalid(format!("{e:?}"))
            }
            other => Self::Other(format!("{other:?}")),
        }
    }
}

/// `GET url`.
///
/// # Errors
///
/// An [`HttpError`]; a response with any status is `Ok`.
pub fn get(url: &str) -> Result<Response, HttpError> {
    send(&Request::new("GET", url))
}

/// `POST url` with `body` of `content_type`.
///
/// # Errors
///
/// An [`HttpError`]; a response with any status is `Ok`.
pub fn post(
    url: &str,
    content_type: &str,
    body: impl Into<Vec<u8>>,
) -> Result<Response, HttpError> {
    send(
        &Request::new("POST", url)
            .header("content-type", content_type)
            .body(body),
    )
}

/// The parts of a URL wasi:http takes.
#[derive(Debug, PartialEq, Eq)]
struct Url<'a> {
    https: bool,
    authority: &'a str,
    path_with_query: &'a str,
}

fn parse_url(url: &str) -> Result<Url<'_>, HttpError> {
    let (https, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(HttpError::Invalid(format!(
            "{url}: not http:// or https://"
        )));
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(end);
    if authority.is_empty() || authority.contains('@') {
        return Err(HttpError::Invalid(format!("{url}: no host, or a user")));
    }
    let path = path.split('#').next().unwrap_or("");
    let path_with_query = match path {
        "" => "/",
        p if p.starts_with('?') => p,
        p => p,
    };
    Ok(Url {
        https,
        authority,
        path_with_query,
    })
}

/// Send a request and read the whole response.
///
/// # Errors
///
/// An [`HttpError`]; a response with any status is `Ok`.
pub fn send(request: &Request) -> Result<Response, HttpError> {
    let url = parse_url(&request.url)?;
    let headers: Vec<(String, Vec<u8>)> = request.headers.clone();
    let fields =
        Fields::from_list(&headers).map_err(|e| HttpError::Invalid(format!("headers: {e:?}")))?;
    let outgoing = OutgoingRequest::new(fields);
    let method = match request.method.as_str() {
        "GET" => Method::Get,
        "HEAD" => Method::Head,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "DELETE" => Method::Delete,
        "OPTIONS" => Method::Options,
        "PATCH" => Method::Patch,
        other => Method::Other(other.to_string()),
    };
    let invalid = |what: &str| HttpError::Invalid(format!("{what}: {}", request.url));
    outgoing
        .set_method(&method)
        .map_err(|()| invalid("method"))?;
    let scheme = if url.https {
        Scheme::Https
    } else {
        Scheme::Http
    };
    outgoing
        .set_scheme(Some(&scheme))
        .map_err(|()| invalid("scheme"))?;
    outgoing
        .set_authority(Some(url.authority))
        .map_err(|()| invalid("host"))?;
    outgoing
        .set_path_with_query(Some(url.path_with_query))
        .map_err(|()| invalid("path"))?;
    let options = request.timeout.map(|t| {
        let options = RequestOptions::new();
        let nanos = u64::try_from(t.as_nanos()).unwrap_or(u64::MAX);
        let _ = options.set_first_byte_timeout(Some(nanos));
        let _ = options.set_between_bytes_timeout(Some(nanos));
        options
    });
    let body = outgoing.body().map_err(|()| invalid("body"))?;
    let pending = outgoing_handler::handle(outgoing, options)?;
    // A refused request closes the body stream: report the refusal rather
    // than the failed write.
    let mut written = Ok(());
    {
        let out = body.write().map_err(|()| invalid("body"))?;
        for chunk in request.body.chunks(4096) {
            if let Err(e) = out.blocking_write_and_flush(chunk) {
                written = Err(HttpError::Other(format!("request body: {e:?}")));
                break;
            }
        }
    }
    if written.is_ok() {
        OutgoingBody::finish(body, None)?;
    } else {
        drop(body);
    }
    pending.subscribe().block();
    let response = match pending.get() {
        Some(Ok(Ok(r))) => r,
        Some(Ok(Err(e))) => return Err(e.into()),
        Some(Err(())) | None => return Err(HttpError::Other("no response".into())),
    };
    written?;
    let status = response.status();
    let headers = response.headers().entries();
    let incoming = response
        .consume()
        .map_err(|()| HttpError::Other("response body taken".into()))?;
    let mut bytes = Vec::new();
    {
        let stream = incoming
            .stream()
            .map_err(|()| HttpError::Other("response body taken".into()))?;
        loop {
            match stream.blocking_read(64 * 1024) {
                Ok(chunk) => bytes.extend_from_slice(&chunk),
                Err(StreamError::Closed) => break,
                Err(StreamError::LastOperationFailed(e)) => return Err(body_error(&e)),
            }
        }
    }
    drop(IncomingBody::finish(incoming));
    Ok(Response {
        status,
        headers,
        body: bytes,
    })
}

/// The error a response body's stream failed with. wasmtime 49 and 50
/// hand the guest no error code for a failure inside a response body (the
/// host's `http-error-code` answers none), so a timeout is recognised by its
/// text.
fn body_error(e: &wasip2::io::error::Error) -> HttpError {
    if let Some(code) = wasip2::http::types::http_error_code(e) {
        return code.into();
    }
    body_error_text(&e.to_debug_string())
}

fn body_error_text(text: &str) -> HttpError {
    if text.contains("timeout") {
        HttpError::Timeout
    } else {
        HttpError::Other(format!("response body: {text}"))
    }
}

#[cfg(test)]
#[path = "http_client_tests.rs"]
mod http_client_tests;
