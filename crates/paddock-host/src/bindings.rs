//! Host bindings for the `paddock:runtime` app world and the host services
//! Paddock implements (vendored WIT under `wit/vendor`).

// Generated code carries no doc comments.
#![allow(missing_docs)]

wasmtime::component::bindgen!({
    path: [
        "../../wit",
        "../../wit/vendor/wasi-logging",
        "../../wit/vendor/wasi-config",
        "../../wit/vendor/wasi-keyvalue",
        "wit",
    ],
    world: "paddock:host/paddock-app",
    imports: { default: async | trappable },
    exports: { default: async },
    with: {
        "wasi:keyvalue/store.bucket": crate::kv::BucketHandle,
    },
});
