//! Bundles the real echo fixture and inspects it. Needs the fixture built
//! first, so it is ignored by default; CI's guest job runs it:
//!
//! ```sh
//! cargo build -p bulkhead-fixture-echo --target wasm32-wasip2 --release
//! cargo test -p bulkhead-bundle --test fixture_echo -- --ignored
//! ```

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::path::PathBuf;

use bulkhead_bundle::{ImportLink, OperatorPolicy, VerifiedBundle};
use common::Spec;
use serde_json::json;
use tempfile::TempDir;

fn echo_wasm() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/release/bulkhead_fixture_echo.wasm");
    std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}; build it with \
             `cargo build -p bulkhead-fixture-echo --target wasm32-wasip2 --release`",
            path.display()
        )
    })
}

#[test]
#[ignore = "needs bulkhead-fixture-echo built for wasm32-wasip2"]
fn echo_fixture____bundled_and_verified____imports_only_runtime_types() {
    let dir = TempDir::new().unwrap();
    let spec = Spec {
        name: "echo",
        version: "0.1.0",
        component: echo_wasm(),
        appserver: Some(json!({})),
        ..Spec::default()
    };
    let bundle = VerifiedBundle::open(spec.write(&dir), &[spec.public_key()]).unwrap();

    let grant = bundle.effective_grant(&OperatorPolicy::default()).unwrap();

    assert_eq!(
        grant.imports.into_iter().collect::<Vec<_>>(),
        vec![(
            "bulkhead:runtime/types@0.1.0".to_string(),
            ImportLink::Implicit
        )]
    );
}
