//! Modbus TCP for Paddock: the `paddock-modbus:modbus` extension.
//!
//! The node polls Modbus TCP devices and casts point updates to apps, and
//! can serve registers to Modbus masters. Protocol work stays native and
//! multi-core in the host; apps get one message per device and scan
//! (DESIGN.md, "Scaling and where code runs").
//!
//! Devices and the server are named in node configuration:
//!
//! ```toml
//! [extensions."paddock-modbus:modbus".devices.pump-1]
//! address = "10.0.0.5:502"
//! unit = 1
//! poll_ms = 1000
//! timeout_ms = 1000
//! full_every_ms = 60000         # an integrity scan; optional
//! deliver_to = "scada"          # the app updates are cast to
//! holding = [{ start = 0, count = 10 }]
//! coils = [{ start = 0, count = 16 }]
//!
//! [extensions."paddock-modbus:modbus".server]
//! listen = "0.0.0.0:5020"
//! deliver_to = "scada"          # masters' writes
//! holding = 100                 # table sizes
//! coils = 64
//! ```
//!
//! Updates are CBOR casts described in [`points`]; give the receiving app
//! `instances.routing = "hash:device"` so each device stays on one
//! instance, in order. Apps read and write devices, and set the served
//! registers, through the `client` and `server` interfaces
//! (`wit/modbus.wit`), declared in the manifest and granted by the
//! operator like any extension:
//!
//! ```toml
//! [appserver.capabilities.extensions."paddock-modbus:modbus"]
//! read = ["pump-1"]
//! write = ["pump-1"]
//! serve = true
//! ```

mod config;
mod device;
pub mod points;
mod server;

/// The server's request handling, for the fuzz targets in `fuzz/` only.
#[cfg(fuzzing)]
#[doc(hidden)]
pub use server::fuzz_request;

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use paddock_host::services::{
    Extension, ExtensionApp, ExtensionCtx, ExtensionNode, ServiceCtx, StoreData, extension_ctx,
};
use paddock_host::wasmtime;
use paddock_host::wasmtime::component::Linker;
use serde::Deserialize;

pub use config::{Config, DeviceConfig, Range, ServerConfig};

use config::{MAX_READ_BITS, MAX_READ_REGISTERS, MAX_WRITE_BITS, MAX_WRITE_REGISTERS};
use device::{DeviceError, DeviceHandle, Reply, Request};
use points::Table;
use server::Served;

mod bindings {
    // Generated code carries no doc comments.
    #![allow(missing_docs)]

    paddock_host::wasmtime::component::bindgen!({
        path: "wit",
        world: "paddock-modbus:modbus/modbus",
        imports: { default: async | trappable },
        wasmtime_crate: paddock_host::wasmtime,
    });
}

use bindings::paddock_modbus::modbus::{client, server as served};

/// The package apps declare and operators grant.
pub const PACKAGE: &str = "paddock-modbus:modbus";

/// The Modbus extension. One per node.
#[derive(Debug, Default, Clone)]
pub struct Modbus {
    shared: Arc<Shared>,
}

/// What the node-level tasks and every instance share.
#[derive(Debug, Default)]
struct Shared {
    devices: OnceLock<BTreeMap<String, DeviceHandle>>,
    served: OnceLock<Arc<Served>>,
    server_address: OnceLock<SocketAddr>,
}

impl Modbus {
    /// The address the server listens on, once started (useful with
    /// `listen` port 0).
    #[must_use]
    pub fn server_address(&self) -> Option<SocketAddr> {
        self.shared.server_address.get().copied()
    }
}

/// The manifest scope.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Scope {
    read: BTreeSet<String>,
    write: BTreeSet<String>,
    serve: bool,
}

/// One instance's state.
#[derive(Debug)]
struct State {
    shared: Arc<Shared>,
    scope: Scope,
    granted: bool,
}

impl Extension for Modbus {
    fn package(&self) -> &str {
        PACKAGE
    }

    fn link(&self, linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
        client::add_to_linker::<StoreData, ExtensionCtx<State>>(linker, extension_ctx::<State>)?;
        served::add_to_linker::<StoreData, ExtensionCtx<State>>(linker, extension_ctx::<State>)
    }

    fn instance(&self, app: &ExtensionApp<'_>) -> Result<Box<dyn Any + Send>, String> {
        let scope = if app.scope.is_null() {
            Scope::default()
        } else {
            serde_json::from_value(app.scope.clone()).map_err(|e| e.to_string())?
        };
        Ok(Box::new(State {
            shared: self.shared.clone(),
            scope,
            granted: app.granted,
        }))
    }

    fn start(&self, node: ExtensionNode) -> Result<(), String> {
        let config = Config::from_json(node.config())?;
        let devices = config
            .devices
            .into_iter()
            .map(|(name, cfg)| {
                let handle = device::spawn(name.clone(), cfg, node.clone());
                (name, handle)
            })
            .collect();
        self.shared
            .devices
            .set(devices)
            .map_err(|_| "started twice".to_string())?;
        if let Some(server) = config.server {
            let tables = Arc::new(Served::new(&server));
            let bound = server::start(server, tables.clone(), node)?;
            let _ = self.shared.served.set(tables);
            let _ = self.shared.server_address.set(bound);
        }
        Ok(())
    }
}

impl State {
    /// The device `name`, if this app may use it (`write` for writes).
    fn device(&self, name: &str, write: bool) -> Result<DeviceHandle, client::Error> {
        let listed = if write {
            self.scope.write.contains(name)
        } else {
            self.scope.read.contains(name) || self.scope.write.contains(name)
        };
        if !self.granted || !listed {
            return Err(client::Error::AccessDenied);
        }
        let devices = self
            .shared
            .devices
            .get()
            .ok_or_else(|| client::Error::Offline("the extension has not started".into()))?;
        devices
            .get(name)
            .cloned()
            .ok_or(client::Error::NoSuchDevice)
    }

    fn served(&self) -> Result<Arc<Served>, served::Error> {
        if !self.granted || !self.scope.serve {
            return Err(served::Error::AccessDenied);
        }
        self.shared
            .served
            .get()
            .cloned()
            .ok_or(served::Error::NotServing)
    }
}

/// Run `request` on `device`, counting the wait as host time.
async fn run(
    ctx: &mut ServiceCtx<'_, State>,
    device: &str,
    request: Request,
) -> Result<Reply, client::Error> {
    let write = matches!(request, Request::WriteWords(..) | Request::WriteBits(..));
    let handle = ctx.state().device(device, write)?;
    let started = Instant::now();
    let result = handle.request(request).await;
    ctx.add_host_time(started.elapsed());
    result.map_err(|e| match e {
        DeviceError::Offline(why) => client::Error::Offline(why),
        DeviceError::Exception(code) => client::Error::Exception(code),
    })
}

async fn read_words(
    ctx: &mut ServiceCtx<'_, State>,
    device: String,
    table: Table,
    address: u16,
    count: u16,
) -> Result<Vec<u16>, client::Error> {
    check_count(count, MAX_READ_REGISTERS)?;
    match run(ctx, &device, Request::ReadWords(table, address, count)).await? {
        Reply::Words(w) => Ok(w),
        other => Err(client::Error::Offline(format!(
            "unexpected reply {other:?}"
        ))),
    }
}

async fn read_bits(
    ctx: &mut ServiceCtx<'_, State>,
    device: String,
    table: Table,
    address: u16,
    count: u16,
) -> Result<Vec<bool>, client::Error> {
    check_count(count, MAX_READ_BITS)?;
    match run(ctx, &device, Request::ReadBits(table, address, count)).await? {
        Reply::Bits(mut b) => {
            b.truncate(usize::from(count));
            Ok(b)
        }
        other => Err(client::Error::Offline(format!(
            "unexpected reply {other:?}"
        ))),
    }
}

fn check_count(count: u16, max: u16) -> Result<(), client::Error> {
    if count == 0 || count > max {
        return Err(client::Error::Invalid(format!(
            "count {count} must be 1 to {max}"
        )));
    }
    Ok(())
}

fn check_len(len: usize, max: usize) -> Result<(), client::Error> {
    if len == 0 || len > max {
        return Err(client::Error::Invalid(format!(
            "{len} values; must be 1 to {max}"
        )));
    }
    Ok(())
}

impl client::Host for ServiceCtx<'_, State> {
    async fn read_holding(
        &mut self,
        device: String,
        address: u16,
        count: u16,
    ) -> wasmtime::Result<Result<Vec<u16>, client::Error>> {
        Ok(read_words(self, device, Table::Holding, address, count).await)
    }

    async fn read_input(
        &mut self,
        device: String,
        address: u16,
        count: u16,
    ) -> wasmtime::Result<Result<Vec<u16>, client::Error>> {
        Ok(read_words(self, device, Table::Input, address, count).await)
    }

    async fn read_coils(
        &mut self,
        device: String,
        address: u16,
        count: u16,
    ) -> wasmtime::Result<Result<Vec<bool>, client::Error>> {
        Ok(read_bits(self, device, Table::Coil, address, count).await)
    }

    async fn read_discrete(
        &mut self,
        device: String,
        address: u16,
        count: u16,
    ) -> wasmtime::Result<Result<Vec<bool>, client::Error>> {
        Ok(read_bits(self, device, Table::Discrete, address, count).await)
    }

    async fn write_holding(
        &mut self,
        device: String,
        address: u16,
        values: Vec<u16>,
    ) -> wasmtime::Result<Result<(), client::Error>> {
        if let Err(e) = check_len(values.len(), MAX_WRITE_REGISTERS) {
            return Ok(Err(e));
        }
        Ok(run(self, &device, Request::WriteWords(address, values))
            .await
            .map(|_| ()))
    }

    async fn write_coils(
        &mut self,
        device: String,
        address: u16,
        values: Vec<bool>,
    ) -> wasmtime::Result<Result<(), client::Error>> {
        if let Err(e) = check_len(values.len(), MAX_WRITE_BITS) {
            return Ok(Err(e));
        }
        Ok(run(self, &device, Request::WriteBits(address, values))
            .await
            .map(|_| ()))
    }
}

fn range(_: server::OutOfRange) -> served::Error {
    served::Error::OutOfRange
}

impl served::Host for ServiceCtx<'_, State> {
    async fn set_holding(
        &mut self,
        address: u16,
        values: Vec<u16>,
    ) -> wasmtime::Result<Result<(), served::Error>> {
        Ok(self
            .state()
            .served()
            .and_then(|s| server::set(&mut s.tables().holding, address, &values).map_err(range)))
    }

    async fn set_input(
        &mut self,
        address: u16,
        values: Vec<u16>,
    ) -> wasmtime::Result<Result<(), served::Error>> {
        Ok(self
            .state()
            .served()
            .and_then(|s| server::set(&mut s.tables().input, address, &values).map_err(range)))
    }

    async fn set_coils(
        &mut self,
        address: u16,
        values: Vec<bool>,
    ) -> wasmtime::Result<Result<(), served::Error>> {
        Ok(self
            .state()
            .served()
            .and_then(|s| server::set(&mut s.tables().coils, address, &values).map_err(range)))
    }

    async fn set_discrete(
        &mut self,
        address: u16,
        values: Vec<bool>,
    ) -> wasmtime::Result<Result<(), served::Error>> {
        Ok(self
            .state()
            .served()
            .and_then(|s| server::set(&mut s.tables().discrete, address, &values).map_err(range)))
    }

    async fn get_holding(
        &mut self,
        address: u16,
        count: u16,
    ) -> wasmtime::Result<Result<Vec<u16>, served::Error>> {
        Ok(self
            .state()
            .served()
            .and_then(|s| server::get(&s.tables().holding, address, count).map_err(range)))
    }

    async fn get_coils(
        &mut self,
        address: u16,
        count: u16,
    ) -> wasmtime::Result<Result<Vec<bool>, served::Error>> {
        Ok(self
            .state()
            .served()
            .and_then(|s| server::get(&s.tables().coils, address, count).map_err(range)))
    }
}
