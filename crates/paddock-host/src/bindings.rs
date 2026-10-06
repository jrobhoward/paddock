//! Host bindings for the `paddock:runtime` app world.

// Generated code carries no doc comments.
#![allow(missing_docs)]

wasmtime::component::bindgen!({
    path: "../../wit",
    world: "paddock:runtime/app",
    imports: { default: async | trappable },
    exports: { default: async },
});
