//! A host service from outside Bulkhead: the `example:greeting` extension.
//!
//! It depends on `bulkhead-host` alone. Apps declare it in their manifest
//! with a scope the service reads:
//!
//! ```toml
//! [appserver.capabilities.extensions."example:greeting"]
//! salutation = "Howdy"
//! ```
//!
//! and the operator grants it by package name (`grant =
//! ["example:greeting"]`). A node runs it with
//! `bulkhead::node::run(&config, vec![Arc::new(Greeter::default())])`.
//!
//! Apps call its functions through the node ([`Extension::call`]), named `greet#hello` and `greet#count`, with
//! arguments and results in CBOR.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bulkhead_host::services::{
    Extension, ExtensionApp, NativeCall, NativeReply, StoreData, extension_ctx, reply,
};
use serde::{Deserialize, Serialize};

/// Why a greeting was refused, in CBOR to apps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Error {
    /// The operator did not grant the package.
    AccessDenied,
}

/// The greeter service. One per node: its count covers every app.
#[derive(Debug, Default)]
pub struct Greeter {
    greetings: Arc<AtomicU64>,
}

/// The greeter's state for one instance.
#[derive(Debug)]
struct State {
    salutation: String,
    granted: bool,
    greetings: Arc<AtomicU64>,
}

impl Extension for Greeter {
    fn package(&self) -> &str {
        "example:greeting"
    }

    fn instance(&self, app: &ExtensionApp<'_>) -> Result<Box<dyn Any + Send>, String> {
        let salutation = match app.scope.get("salutation") {
            None => "Hello".to_string(),
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(_) => return Err("salutation must be a string".into()),
        };
        Ok(Box::new(State {
            salutation,
            granted: app.granted,
            greetings: self.greetings.clone(),
        }))
    }

    /// `greet#hello` and `greet#count`.
    fn call<'a>(&'a self, data: &'a mut StoreData, call: NativeCall) -> NativeReply<'a> {
        Box::pin(async move {
            let mut ctx = extension_ctx::<State>(data);
            let app = ctx.app().to_string();
            let state = ctx.state();
            match call.function() {
                "greet#hello" => {
                    let (name,): (String,) = call.args()?;
                    reply(&state.hello(&app, &name))
                }
                "greet#count" => reply(&state.count()),
                _ => Err(call.no_such_function()),
            }
        })
    }
}

impl State {
    fn hello(&self, app: &str, name: &str) -> Result<String, Error> {
        if !self.granted {
            return Err(Error::AccessDenied);
        }
        self.greetings.fetch_add(1, Ordering::Relaxed);
        Ok(format!("{}, {name} (from {app})", self.salutation))
    }

    fn count(&self) -> Result<u64, Error> {
        if !self.granted {
            return Err(Error::AccessDenied);
        }
        Ok(self.greetings.load(Ordering::Relaxed))
    }
}
