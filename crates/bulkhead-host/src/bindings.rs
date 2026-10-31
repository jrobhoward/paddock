//! Host bindings for the `bulkhead:runtime` app world: the callbacks and
//! `process`. Each host service has its own bindings (`src/services`).

// Generated code carries no doc comments.
#![allow(missing_docs)]

wasmtime::component::bindgen!({
    path: [
        "../../wit",
        "../../wit/vendor/wasi-logging",
        "../../wit/vendor/wasi-config",
        "../../wit/vendor/wasi-keyvalue",
        "../../wit/groups",
        "../../wit/watch",
        "../../wit/secrets",
        "../../wit/metrics",
            "../../wit/sql",
        "wit",
    ],
    world: "bulkhead:host/bulkhead-app",
    imports: { default: async | trappable },
    exports: { default: async },
});
