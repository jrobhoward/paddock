//! The extension's node-level side through a runtime: the server answering
//! a real Modbus client, and configuration the start refuses.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use paddock_host::services::Extension;
use paddock_host::{Runtime, RuntimeConfig};
use paddock_modbus::{Modbus, PACKAGE};
use serde_json::json;
use tokio_modbus::client::{Reader, Writer};
use tokio_modbus::prelude::Slave;

fn runtime(modbus: &Modbus) -> Runtime {
    let ext: Arc<dyn Extension> = Arc::new(modbus.clone());
    Runtime::with_extensions(RuntimeConfig::default(), vec![ext]).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server____modbus_client____reads_and_writes_the_tables() {
    let modbus = Modbus::default();
    let rt = runtime(&modbus);
    let apps = rt.start_apps(Vec::new()).await.unwrap();
    let config = json!({ "server": { "listen": "127.0.0.1:0", "holding": 4, "coils": 8 } });
    let started = rt
        .start_extensions(&apps, &BTreeMap::from([(PACKAGE.to_string(), config)]))
        .unwrap();
    let address = modbus.server_address().unwrap();
    let mut client = tokio_modbus::client::tcp::connect_slave(address, Slave(1))
        .await
        .unwrap();

    client
        .write_multiple_registers(1, &[11, 12])
        .await
        .unwrap()
        .unwrap();
    let read = client.read_holding_registers(0, 4).await.unwrap().unwrap();
    let past = client.read_holding_registers(3, 2).await.unwrap();
    client.write_single_coil(7, true).await.unwrap().unwrap();
    let coils = client.read_coils(6, 2).await.unwrap().unwrap();

    assert_eq!(read, vec![0, 11, 12, 0]);
    assert_eq!(past, Err(tokio_modbus::ExceptionCode::IllegalDataAddress));
    assert_eq!(&coils[..2], &[false, true]);
    started.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server____master_not_in_allow____refused() {
    let modbus = Modbus::default();
    let rt = runtime(&modbus);
    let apps = rt.start_apps(Vec::new()).await.unwrap();
    let config = json!({ "server": {
        "listen": "127.0.0.1:0", "holding": 4, "allow": ["10.0.0.0/8"]
    } });
    let started = rt
        .start_extensions(&apps, &BTreeMap::from([(PACKAGE.to_string(), config)]))
        .unwrap();
    let address = modbus.server_address().unwrap();

    let read = match tokio_modbus::client::tcp::connect_slave(address, Slave(1)).await {
        Ok(mut client) => tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.read_holding_registers(0, 1),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(Result::ok),
        Err(_) => None,
    };

    assert_eq!(read, None);
    started.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start____bad_configuration____refused() {
    let modbus = Modbus::default();
    let rt = runtime(&modbus);
    let apps = rt.start_apps(Vec::new()).await.unwrap();
    let start = |package: &str, config: serde_json::Value| {
        rt.start_extensions(&apps, &BTreeMap::from([(package.to_string(), config)]))
            .map(|_| ())
            .map_err(|e| e.to_string())
    };

    let unknown_field = start(PACKAGE, json!({ "devcies": {} }));
    let bad_allow = start(
        PACKAGE,
        json!({ "server": { "listen": "127.0.0.1:0", "allow": ["10.0.0.0/33"] } }),
    );
    let unknown_extension = start("other:ext", json!({}));
    let no_target = start(
        PACKAGE,
        json!({ "devices": { "d": {
            "address": "127.0.0.1:1", "holding": [{ "start": 0, "count": 1 }]
        } } }),
    );

    assert!(unknown_field.unwrap_err().contains("devcies"));
    assert!(bad_allow.unwrap_err().contains("server.allow"));
    assert!(unknown_extension.unwrap_err().contains("other:ext"));
    assert!(no_target.unwrap_err().contains("deliver_to"));
}
