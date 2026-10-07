//! Spike S10: does a host whose WIT has one more `info` case still load a
//! guest built against the original `paddock:runtime@0.1.0`?

use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

mod extra {
    wasmtime::component::bindgen!({
        path: "wit-extra",
        world: "paddock:runtime/app",
        imports: { default: async | trappable },
        exports: { default: async },
    });
}

struct State {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

impl extra::paddock::runtime::types::Host for State {}

#[tokio::main]
async fn main() -> wasmtime::Result<()> {
    let wasm = std::env::args().nth(1).expect("usage: s10-host <echo.wasm>");
    let engine = Engine::new(&Config::new())?;
    let component = Component::from_file(&engine, &wasm)?;
    let mut linker: Linker<State> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    // Echo imports only `types`; link it with the extended package.
    extra::paddock::runtime::types::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;

    let pre = linker.instantiate_pre(&component);
    println!("instantiate_pre: {}", pre.as_ref().map(|_| "ok".to_string()).unwrap_or_else(|e| format!("{e:#}")));
    let Ok(pre) = pre else { return Ok(()) };
    let typed = extra::AppPre::new(pre);
    println!("AppPre::new (export types checked): {}", typed.as_ref().map(|_| "ok".to_string()).unwrap_or_else(|e| format!("{e:#}")));
    if let Ok(typed) = typed {
        let mut store = Store::new(&engine, State { wasi: WasiCtx::builder().build(), table: ResourceTable::new() });
        let app = typed.instantiate_async(&mut store).await?;
        let info = extra::paddock::runtime::types::Info::Timer((1, extra::paddock::runtime::types::Envelope { content_type: "t".into(), schema: None, body: vec![] }));
        println!("handle-info(timer): {:?}", app.call_handle_info(&mut store, &info).await.map(|_| "ok"));
        let new = extra::paddock::runtime::types::Info::NodeDown(3);
        println!("handle-info(node-down): {:?}", app.call_handle_info(&mut store, &new).await.map(|_| "ok").map_err(|e| format!("{e:#}")));
    }
    Ok(())
}
