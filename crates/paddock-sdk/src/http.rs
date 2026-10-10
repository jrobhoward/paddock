//! HTTP requests from the node's ingress (cargo feature `http`).
//!
//! The node terminates HTTP and sends each request on the app's routes
//! (manifest `capabilities."wasi:http/incoming-handler".routes`) to the app
//! as a call. In `handle_call`, [`Request::from_envelope`] recognises one;
//! answer with a [`Response`], now or later with `Ctx::reply`. Any other
//! reply is sent as `200` with its content type and body.
//!
//! ```ignore
//! use paddock_sdk::http::{Request, Response};
//!
//! fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
//!     if let Some(req) = Request::from_envelope(&msg) {
//!         return Reply::Now(match (req.method.as_str(), req.path.as_str()) {
//!             ("GET", "/hello") => Response::text(200, "hello"),
//!             _ => Response::text(404, "not here"),
//!         }.into());
//!     }
//!     // ... other calls
//! }
//! ```

use ciborium::Value;
use serde::Serialize;

use crate::envelope::{Envelope, MessageError};

/// The `schema` of a request envelope.
pub const REQUEST_SCHEMA: &str = "paddock/http-request";
/// The `schema` of a response envelope.
pub const RESPONSE_SCHEMA: &str = "paddock/http-response";

/// An HTTP request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Request {
    /// `http` or `https`: how the client reached the node.
    pub scheme: String,
    /// `GET`, `POST`, ...
    pub method: String,
    /// The path, without the query.
    pub path: String,
    /// The query string, without `?`.
    pub query: Option<String>,
    /// Headers in order, names lowercase.
    pub headers: Vec<(String, String)>,
    /// The whole body.
    pub body: Vec<u8>,
    /// The value of the route's `hash_header`, if it names one.
    pub key: Option<String>,
}

fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn field<'a>(map: &'a [(Value, Value)], name: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_text() == Some(name))
        .map(|(_, v)| v)
}

fn pairs(value: Option<&Value>) -> Vec<(String, String)> {
    value
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|p| {
                    let p = p.as_array()?;
                    Some((
                        p.first()?.as_text()?.to_string(),
                        p.get(1)?.as_text()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Request {
    /// The request `msg` carries, if it is one.
    #[must_use]
    pub fn from_envelope(msg: &Envelope) -> Option<Self> {
        if msg.schema.as_deref() != Some(REQUEST_SCHEMA) {
            return None;
        }
        let value: Value = ciborium::from_reader(msg.body.as_slice()).ok()?;
        let map = value.as_map()?;
        let get_text = |name| {
            field(map, name)
                .and_then(Value::as_text)
                .map(str::to_string)
        };
        Some(Self {
            scheme: get_text("scheme").unwrap_or_else(|| "http".into()),
            method: get_text("method")?,
            path: get_text("path")?,
            query: get_text("query"),
            headers: pairs(field(map, "headers")),
            body: field(map, "body")
                .and_then(Value::as_bytes)
                .cloned()
                .unwrap_or_default(),
            key: get_text("key"),
        })
    }

    /// The first header named `name` (any case).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body as UTF-8 text.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.body).ok()
    }

    /// The query parameter `name`, undecoded.
    #[must_use]
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .as_deref()?
            .split('&')
            .filter_map(|kv| kv.split_once('=').or(Some((kv, ""))))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }

    /// The request as an envelope, as the node sends it (for tests).
    #[must_use]
    pub fn to_envelope(&self) -> Envelope {
        let headers = self
            .headers
            .iter()
            .map(|(k, v)| Value::Array(vec![text(k), text(v)]))
            .collect();
        let mut map = vec![
            (text("scheme"), text(&self.scheme)),
            (text("method"), text(&self.method)),
            (text("path"), text(&self.path)),
            (
                text("query"),
                self.query.clone().map_or(Value::Null, Value::Text),
            ),
            (text("headers"), Value::Array(headers)),
            (text("body"), Value::Bytes(self.body.clone())),
        ];
        if let Some(key) = &self.key {
            map.push((text("key"), text(key)));
        }
        encode(&Value::Map(map), REQUEST_SCHEMA)
    }
}

fn encode(value: &Value, schema: &str) -> Envelope {
    let mut body = Vec::new();
    // Writing a CBOR value to a Vec cannot fail.
    let _ = ciborium::into_writer(value, &mut body);
    Envelope::new("application/cbor", body).with_schema(schema)
}

/// An HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// Headers in order.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Response {
    /// A response with `status` and no body.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// A `text/plain` response.
    #[must_use]
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::new(status)
            .header("content-type", "text/plain; charset=utf-8")
            .body(body.into().into_bytes())
    }

    /// An `application/json` response.
    ///
    /// # Errors
    ///
    /// If `value` does not serialise.
    pub fn json<T: Serialize + ?Sized>(status: u16, value: &T) -> Result<Self, MessageError> {
        let body = serde_json::to_vec(value).map_err(|e| MessageError(e.to_string()))?;
        Ok(Self::new(status)
            .header("content-type", "application/json")
            .body(body))
    }

    /// Add a header.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Set the body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// The response as the envelope the node expects.
    #[must_use]
    pub fn to_envelope(&self) -> Envelope {
        let headers = self
            .headers
            .iter()
            .map(|(k, v)| Value::Array(vec![text(k), text(v)]))
            .collect();
        let map = vec![
            (text("status"), Value::Integer(self.status.into())),
            (text("headers"), Value::Array(headers)),
            (text("body"), Value::Bytes(self.body.clone())),
        ];
        encode(&Value::Map(map), RESPONSE_SCHEMA)
    }

    /// The response an envelope carries, if it is one (for tests).
    #[must_use]
    pub fn from_envelope(msg: &Envelope) -> Option<Self> {
        if msg.schema.as_deref() != Some(RESPONSE_SCHEMA) {
            return None;
        }
        let value: Value = ciborium::from_reader(msg.body.as_slice()).ok()?;
        let map = value.as_map()?;
        Some(Self {
            status: field(map, "status")
                .and_then(Value::as_integer)
                .and_then(|i| u16::try_from(i).ok())?,
            headers: pairs(field(map, "headers")),
            body: field(map, "body")
                .and_then(Value::as_bytes)
                .cloned()
                .unwrap_or_default(),
        })
    }
}

impl From<Response> for Envelope {
    fn from(r: Response) -> Self {
        r.to_envelope()
    }
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod http_tests;
