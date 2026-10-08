//! A host service from outside Paddock: the `example:greeting` extension.
//!
//! It depends on `paddock-host` alone. Apps declare it in their manifest
//! with a scope the service reads:
//!
//! ```toml
//! [appserver.capabilities.extensions."example:greeting"]
//! salutation = "Howdy"
//! ```
//!
//! and the operator grants it by package name (`grant =
//! ["example:greeting"]`). A node runs it with
//! `paddock::node::run(&config, vec![Arc::new(Greeter::default())])`.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use paddock_host::services::{
    Extension, ExtensionApp, ExtensionCtx, ServiceCtx, StoreData, extension_ctx,
};
use paddock_host::wasmtime;
use paddock_host::wasmtime::component::Linker;

mod bindings {
    // Generated code carries no doc comments.
    #![allow(missing_docs)]

    paddock_host::wasmtime::component::bindgen!({
        path: "../wit",
        world: "example:greeting/client",
        imports: { default: async | trappable },
        // The wasmtime paddock-host links against; no wasmtime dependency
        // of this crate's own, so the versions cannot drift.
        wasmtime_crate: paddock_host::wasmtime,
    });
}

use bindings::example::greeting::greet;

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

    fn link(&self, linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
        greet::add_to_linker::<StoreData, ExtensionCtx<State>>(linker, extension_ctx::<State>)
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
}

impl greet::Host for ServiceCtx<'_, State> {
    async fn hello(&mut self, name: String) -> wasmtime::Result<Result<String, greet::Error>> {
        let app = self.app().to_string();
        let state = self.state();
        if !state.granted {
            return Ok(Err(greet::Error::AccessDenied));
        }
        state.greetings.fetch_add(1, Ordering::Relaxed);
        Ok(Ok(format!("{}, {name} (from {app})", state.salutation)))
    }

    async fn count(&mut self) -> wasmtime::Result<Result<u64, greet::Error>> {
        let state = self.state();
        if !state.granted {
            return Ok(Err(greet::Error::AccessDenied));
        }
        Ok(Ok(state.greetings.load(Ordering::Relaxed)))
    }
}
