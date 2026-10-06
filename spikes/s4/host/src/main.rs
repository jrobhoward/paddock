//! Spike S4: inspect a component's imports at deploy time, refuse undeclared
//! imports, and link declared-but-ungranted imports to deny stubs.
//!
//! Effective permission = component imports ∩ manifest declaration ∩ operator
//! grant. Granted custom interfaces get real host implementations; ungranted
//! ones get generic stubs built from the function's type at call time:
//!
//! - result<_, E> where E has a `permission-denied` case → return that error
//! - result<_, string> → return "permission-denied"
//! - anything else → trap
//!
//! WASI interfaces are always linked (Rust std imports them) and restricted by
//! configuration instead: no preopens, read-only preopens, socket checks.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use wasmtime::component::types::{ComponentFunc, ComponentItem};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable, Type, Val};
use wasmtime::{Engine, Error, Result, Store};
use wasmtime_wasi::sockets::SocketAddrUse;
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};

wasmtime::component::bindgen!({ path: "../wit", world: "app" });

use paddock::spike4::{audit, kv, net};

const KV: &str = "paddock:spike4/kv@0.1.0";
const NET: &str = "paddock:spike4/net@0.1.0";
const AUDIT: &str = "paddock:spike4/audit@0.1.0";

struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    kv: HashMap<String, String>,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

impl kv::Host for State {
    fn get(&mut self, key: String) -> Result<Option<String>, kv::Error> {
        Ok(self.kv.get(&key).cloned())
    }
    fn set(&mut self, key: String, value: String) -> Result<(), kv::Error> {
        self.kv.insert(key, value);
        Ok(())
    }
}

impl net::Host for State {
    fn fetch(&mut self, url: String) -> Result<String, net::Error> {
        Ok(format!("200 from {url}"))
    }
}

impl audit::Host for State {
    fn record_event(&mut self, msg: String) {
        println!("      [audit] {msg}");
    }
}

/// What the operator allows for one deployment.
struct Policy {
    name: &'static str,
    granted: BTreeSet<&'static str>,
    /// (host dir, guest path, writable)
    preopens: Vec<(PathBuf, &'static str, bool)>,
    tcp: bool,
    /// Loopback port of a host listener the guest may connect to.
    port: u16,
    /// Use wasmtime's built-in trap stubs instead of the typed deny stubs.
    trap_stubs_only: bool,
}

/// Deploy-time check: list imports and refuse anything the manifest doesn't declare.
fn check_imports(engine: &Engine, component: &Component, declared: &[&str]) -> Result<Vec<String>> {
    let imports: Vec<String> =
        component.component_type().imports(engine).map(|(name, _)| name.to_string()).collect();
    let undeclared: Vec<&String> = imports
        .iter()
        .filter(|name| !name.starts_with("wasi:") && !declared.contains(&name.as_str()))
        .collect();
    if undeclared.is_empty() {
        Ok(imports)
    } else {
        Err(Error::msg(format!("refused: imports not declared in manifest: {undeclared:?}")))
    }
}

/// Build the error value a deny stub returns for this function type, if any.
fn denied_value(ty: &ComponentFunc) -> Option<Val> {
    let mut results = ty.results();
    let (Some(Type::Result(r)), None) = (results.next(), results.next()) else {
        return None;
    };
    let err = match r.err()? {
        Type::Variant(v) if v.cases().any(|c| c.name == "permission-denied" && c.ty.is_none()) => {
            Val::Variant("permission-denied".into(), None)
        }
        Type::Enum(e) if e.names().any(|n| n == "permission-denied") => Val::Enum("permission-denied".into()),
        Type::String => Val::String("permission-denied".into()),
        _ => return None,
    };
    Some(Val::Result(Err(Some(Box::new(err)))))
}

/// Link every function of an ungranted import instance to a deny stub.
fn link_deny_stubs(linker: &mut Linker<State>, engine: &Engine, component: &Component, iface: &str) -> Result<()> {
    let ty = component.component_type();
    let Some(ext) = ty.get_import(engine, iface) else { return Ok(()) };
    let ComponentItem::ComponentInstance(inst) = ext.ty else { return Ok(()) };
    let mut li = linker.instance(iface)?;
    for (func_name, item) in inst.exports(engine) {
        if !matches!(item.ty, ComponentItem::ComponentFunc(_)) {
            continue;
        }
        let label = format!("{iface}#{func_name}");
        li.func_new(func_name, move |_store, fty, _params, results| match denied_value(&fty) {
            Some(v) => {
                results[0] = v;
                Ok(())
            }
            None => Err(Error::msg(format!("permission denied: {label} (no error channel, trapping)"))),
        })?;
    }
    Ok(())
}

fn instantiate(engine: &Engine, component: &Component, policy: &Policy) -> Result<(Store<State>, App)> {
    let mut linker: Linker<State> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
    for iface in [KV, NET, AUDIT] {
        if policy.granted.contains(iface) {
            match iface {
                KV => kv::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?,
                NET => net::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?,
                _ => audit::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?,
            }
        } else if !policy.trap_stubs_only {
            link_deny_stubs(&mut linker, engine, component, iface)?;
        }
    }
    if policy.trap_stubs_only {
        linker.define_unknown_imports_as_traps(component)?;
    }

    let mut wasi = WasiCtx::builder();
    wasi.inherit_stderr().allow_tcp(policy.tcp).env("PORT", policy.port.to_string());
    let tcp = policy.tcp;
    // Only loopback, and only if TCP is granted at all.
    // Policy is per use: std binds 0.0.0.0:0 before every connect, so allow
    // ephemeral client binds and restrict connect destinations to loopback.
    wasi.socket_addr_check(move |addr, usage| {
        let ok = tcp
            && match usage {
                SocketAddrUse::TcpBind => addr.port() == 0 && (addr.ip().is_unspecified() || addr.ip().is_loopback()),
                SocketAddrUse::TcpConnect => addr.ip().is_loopback(),
                _ => false,
            };
        if std::env::var("TRACE_SOCK").is_ok() {
            println!("      [socket check] {addr} {usage:?} -> {ok}");
        }
        Box::pin(async move { ok })
    });
    for (host, guest, writable) in &policy.preopens {
        let perms = if *writable { FsPerms::ReadWrite } else { FsPerms::ReadOnly };
        wasi.preopened_dir(host, guest, perms)?;
    }
    let state = State { wasi: wasi.build(), table: ResourceTable::new(), kv: HashMap::new() };
    let mut store = Store::new(engine, state);
    let app = App::instantiate(&mut store, component, &linker)?;
    Ok((store, app))
}

fn run_tests(engine: &Engine, component: &Component, policy: &Policy) -> Result<()> {
    println!("\n== deployment {:?} granted={:?} tcp={} trap_stubs_only={}", policy.name, policy.granted, policy.tcp, policy.trap_stubs_only);
    let (mut store, mut app) = match instantiate(engine, component, policy) {
        Ok(x) => x,
        Err(e) => {
            println!("  instantiate failed: {e}");
            for cause in e.chain().skip(1) {
                println!("    caused by: {cause}");
            }
            return Ok(());
        }
    };
    for test in ["kv", "net", "fs-read", "fs-write", "fs-priv-write", "fs-escape", "tcp", "env", "audit"] {
        match app.call_run(&mut store, test) {
            Ok(out) => println!("  {test:<14} {out}"),
            Err(e) => {
                println!("  {test:<14} TRAP: {}", e.root_cause());
                // A trapped instance is poisoned; rebuild for the next test.
                (store, app) = instantiate(engine, component, policy)?;
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let wasm = std::env::args().nth(1).ok_or_else(|| Error::msg("usage: s4-host <guest.wasm>"))?;
    let engine = Engine::default();
    let component = Component::from_file(&engine, &wasm)?;

    // Host directories for the jailed filesystem.
    let base = std::env::temp_dir().join(format!("paddock-s4-{}", std::process::id()));
    let data = base.join("data");
    let privd = base.join("priv");
    std::fs::create_dir_all(&data)?;
    std::fs::create_dir_all(&privd)?;
    std::fs::write(data.join("hello.txt"), "hello from the host")?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || for _ in listener.incoming() {});

    println!("== deploy-time import inspection");
    let declared_partial = [KV, NET];
    match check_imports(&engine, &component, &declared_partial) {
        Ok(_) => println!("  unexpectedly accepted"),
        Err(e) => println!("  manifest declares kv+net only -> {e}"),
    }
    let imports = check_imports(&engine, &component, &[KV, NET, AUDIT])?;
    println!("  manifest declares kv+net+audit -> accepted; component imports {} interfaces:", imports.len());
    for name in &imports {
        println!("    {name}");
    }

    let full = Policy {
        name: "everything granted",
        granted: [KV, NET, AUDIT].into(),
        preopens: vec![(data.clone(), "/data", true), (privd.clone(), "/priv", false)],
        tcp: true,
        port,
        trap_stubs_only: false,
    };
    let none = Policy {
        name: "nothing granted, typed deny stubs",
        granted: BTreeSet::new(),
        preopens: vec![],
        tcp: false,
        port,
        trap_stubs_only: false,
    };
    let traps = Policy {
        name: "nothing granted, wasmtime trap stubs",
        granted: BTreeSet::new(),
        preopens: vec![],
        tcp: false,
        port,
        trap_stubs_only: true,
    };
    for p in [&full, &none, &traps] {
        run_tests(&engine, &component, p)?;
    }
    println!("\n  host sees /data/out.txt = {:?}", std::fs::read_to_string(data.join("out.txt")));
    std::fs::remove_dir_all(&base)?;
    Ok(())
}
