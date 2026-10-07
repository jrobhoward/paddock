//! Messages: bytes plus how to decode them.

use std::fmt;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// `application/cbor`, the default encoding.
pub const CBOR: &str = "application/cbor";
/// `application/json`.
pub const JSON: &str = "application/json";
/// `text/plain`, UTF-8.
pub const TEXT: &str = "text/plain";

/// A message: an encoded body, its content type, and an optional schema id
/// from the app's message catalog. The host passes all three through
/// untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// How `body` is encoded.
    pub content_type: String,
    /// Schema id, if any.
    pub schema: Option<String>,
    /// The encoded message.
    pub body: Vec<u8>,
}

/// A message could not be encoded or decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageError(pub String);

impl fmt::Display for MessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MessageError {}

impl Envelope {
    /// A message of any content type.
    #[must_use]
    pub fn new(content_type: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            content_type: content_type.into(),
            schema: None,
            body: body.into(),
        }
    }

    /// A `text/plain` message.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::new(TEXT, text.into().into_bytes())
    }

    /// `value` encoded as CBOR.
    ///
    /// # Errors
    ///
    /// [`MessageError`] if `value` cannot be serialized.
    pub fn cbor<T: Serialize + ?Sized>(value: &T) -> Result<Self, MessageError> {
        let mut body = Vec::new();
        ciborium::into_writer(value, &mut body)
            .map_err(|e| MessageError(format!("CBOR encode: {e}")))?;
        Ok(Self::new(CBOR, body))
    }

    /// `value` encoded as JSON.
    ///
    /// # Errors
    ///
    /// [`MessageError`] if `value` cannot be serialized.
    pub fn json<T: Serialize + ?Sized>(value: &T) -> Result<Self, MessageError> {
        let body =
            serde_json::to_vec(value).map_err(|e| MessageError(format!("JSON encode: {e}")))?;
        Ok(Self::new(JSON, body))
    }

    /// The same message with a schema id.
    #[must_use]
    pub fn with_schema(mut self, schema: impl Into<String>) -> Self {
        self.schema = Some(schema.into());
        self
    }

    /// The body as text, if it is `text/plain` (or has no parameters beyond
    /// a charset) and valid UTF-8.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        (media_type(&self.content_type) == TEXT)
            .then(|| std::str::from_utf8(&self.body).ok())
            .flatten()
    }

    /// Decode the body by its content type: CBOR or JSON.
    ///
    /// # Errors
    ///
    /// [`MessageError`] for another content type or a body that does not
    /// decode as `T`.
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T, MessageError> {
        match media_type(&self.content_type) {
            CBOR => ciborium::from_reader(self.body.as_slice())
                .map_err(|e| MessageError(format!("CBOR decode: {e}"))),
            JSON => serde_json::from_slice(&self.body)
                .map_err(|e| MessageError(format!("JSON decode: {e}"))),
            other => Err(MessageError(format!(
                "cannot decode content type '{other}'"
            ))),
        }
    }
}

/// The content type without parameters (`text/plain; charset=utf-8` →
/// `text/plain`).
fn media_type(content_type: &str) -> &str {
    content_type.split(';').next().unwrap_or("").trim()
}

impl From<Envelope> for crate::bindings::Envelope {
    fn from(e: Envelope) -> Self {
        Self {
            content_type: e.content_type,
            schema: e.schema,
            body: e.body,
        }
    }
}

impl From<crate::bindings::Envelope> for Envelope {
    fn from(e: crate::bindings::Envelope) -> Self {
        Self {
            content_type: e.content_type,
            schema: e.schema,
            body: e.body,
        }
    }
}

#[cfg(test)]
#[path = "envelope_tests.rs"]
mod envelope_tests;
