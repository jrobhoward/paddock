//! The extension from a native program (ROADMAP Z8h): `bulkhead-host`'s
//! native fixture calls the `client` and `server` functions through the
//! node, and a real Modbus client reads what it set.

#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::services::{Extension, ExtensionSpec};
use bulkhead_host::{
    Envelope, InstanceHandle, InstanceSpec, NativeCode, NativeConfig, Runtime, RuntimeConfig,
};
use bulkhead_modbus::{Modbus, PACKAGE};
use serde_json::json;
use tokio_modbus::client::Reader;
use tokio_modbus::prelude::Slave;

/// `bulkhead-runner` and `bulkhead-host`'s native fixture, built natively
/// into the target directory `bulkhead-host`'s tests use.
fn native_fixtures() -> (PathBuf, PathBuf) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = root.join("target/fixtures/native");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(&root)
        .args([
            "build",
            "-p",
            "bulkhead-native",
            "-p",
            "bulkhead-fixture-native",
            "--target-dir",
        ])
        .arg(&target)
        .status()
        .unwrap();
    assert!(status.success(), "building native fixtures failed");
    let debug = target.join("debug");
    (
        debug.join("bulkhead-runner"),
        debug.join("bulkhead-fixture-native"),
    )
}

async fn ask(instance: &InstanceHandle, function: &str, args: &str) -> String {
    let command = format!("ext {PACKAGE} {function} {args}");
    let reply = instance
        .call(
            Envelope::new("text/plain", command.as_str()),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    String::from_utf8(reply.body).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_app____client_and_server_functions____as_a_components_imports() {
    let (runner, fixture) = native_fixtures();
    let modbus = Modbus::default();
    let mut native = NativeConfig::default();
    native.runner = Some(runner);
    let ext: Arc<dyn Extension> = Arc::new(modbus.clone());
    let rt = Runtime::with_extensions(
        RuntimeConfig {
            native,
            ..RuntimeConfig::default()
        },
        vec![ext],
    )
    .unwrap();
    let apps = rt.start_apps(Vec::new()).await.unwrap();
    let config = json!({ "server": { "listen": "127.0.0.1:0", "holding": 4, "coils": 8 } });
    let started = rt
        .start_extensions(&apps, &BTreeMap::from([(PACKAGE.to_string(), config)]))
        .unwrap();
    let app = rt.load_native(NativeCode {
        path: fixture,
        kind: NativeProgram::Executable,
    });
    let mut spec = InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        ..InstanceSpec::new("scada", "1.0.0")
    };
    spec.services.extensions.insert(
        PACKAGE.into(),
        ExtensionSpec::new(json!({ "read": ["ghost"], "serve": true })),
    );
    let spec = spec.with_granted_extensions(&BTreeSet::from([PACKAGE.to_string()]));
    let instance = rt.spawn(&app, spec).unwrap();

    let set = ask(&instance, "server#set-holding", "[1,[11,12]]").await;
    let past = ask(&instance, "server#set-coils", "[7,[true,true]]").await;
    let got = ask(&instance, "server#get-holding", "[0,4]").await;
    let mut client =
        tokio_modbus::client::tcp::connect_slave(modbus.server_address().unwrap(), Slave(1))
            .await
            .unwrap();
    let read = client.read_holding_registers(0, 4).await.unwrap().unwrap();
    let unlisted = ask(&instance, "client#read-holding", r#"["pump",0,1]"#).await;
    let unconfigured = ask(&instance, "client#read-holding", r#"["ghost",0,1]"#).await;
    let too_many = ask(&instance, "client#read-coils", r#"["ghost",0,0]"#).await;
    let unwritable = ask(&instance, "client#write-holding", r#"["ghost",0,[1]]"#).await;

    assert_eq!(set, r#"{"Ok":null}"#);
    assert_eq!(past, r#"{"Err":"OutOfRange"}"#);
    assert_eq!(got, r#"{"Ok":[0,11,12,0]}"#);
    assert_eq!(read, vec![0, 11, 12, 0]);
    assert_eq!(unlisted, r#"{"Err":"AccessDenied"}"#);
    assert_eq!(unconfigured, r#"{"Err":"NoSuchDevice"}"#);
    assert!(too_many.starts_with(r#"{"Err":{"Invalid":"#), "{too_many}");
    assert_eq!(unwritable, r#"{"Err":"AccessDenied"}"#);
    started.stop();
}
