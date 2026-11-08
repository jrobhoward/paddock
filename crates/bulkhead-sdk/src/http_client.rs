//! HTTP requests from the app (`wasi:http/outgoing-handler`), blocking.
//!
//! The node sends a request only to a scheme, host and port the manifest
//! lists in `capabilities."wasi:http/outgoing-handler".allow`, and refuses
//! the rest with [`HttpError::Denied`]. Waiting for the response does not
//! count against the callback deadline, but the instance handles no other
//! message meanwhile; a pool of instances keeps an app responsive.
//!
//! ```ignore
//! use bulkhead_sdk::http_client;
//!
//! let reply = http_client::get("https://api.example.com/v1/status")?;
//! if reply.status == 200 {
//!     let text = String::from_utf8_lossy(&reply.body);
//! }
//! ```

use std::time::Duration;

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

/// The parts of a URL.
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

/// Send `request` and wait for the whole response: the node sends it
/// through its egress client on the manifest's rules, and answers with the
/// response, whose body may be up to 16 MiB.
///
/// # Errors
///
/// As [`HttpError`] lists.
pub fn send(request: &Request) -> Result<Response, HttpError> {
    use bulkhead_native::protocol as p;
    parse_url(&request.url)?;
    let asked = p::HttpRequest {
        method: request.method.clone(),
        url: request.url.clone(),
        headers: request
            .headers
            .iter()
            .map(|(n, v)| (n.clone(), p::Bytes(v.clone())))
            .collect(),
        body: p::Bytes(request.body.clone()),
        timeout_ms: request
            .timeout
            .map(|t| u32::try_from(t.as_millis()).unwrap_or(u32::MAX)),
    };
    match bulkhead_native::app::Ctx::current().http(asked) {
        Ok(r) => Ok(Response {
            status: r.status,
            headers: r.headers.into_iter().map(|(n, v)| (n, v.0)).collect(),
            body: r.body.0,
        }),
        Err(e) => Err(match e {
            p::HttpError::Denied => HttpError::Denied,
            p::HttpError::Limited => HttpError::Limited,
            p::HttpError::Prohibited => HttpError::Prohibited,
            p::HttpError::Timeout => HttpError::Timeout,
            p::HttpError::Unreachable(why) => HttpError::Unreachable(why),
            p::HttpError::Tls(why) => HttpError::Tls(why),
            p::HttpError::Invalid(why) => HttpError::Invalid(why),
            p::HttpError::Other(why) => HttpError::Other(why),
        }),
    }
}

#[cfg(test)]
#[path = "http_client_tests.rs"]
mod http_client_tests;
