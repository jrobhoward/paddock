# Greeter: a host service from outside Bulkhead

An extension is a host service built into a node by its operator, serving one WIT package that apps import. This example has four parts:

- `wit/greeting.wit`: the package, `example:greeting`, with `hello(name)` and `count()`.
- `service/`: the extension. It depends on `bulkhead-host` alone.
- `app/`: an app that calls it, written with `bulkhead-sdk` plus a client module for the package (`src/greeting.rs`), so it builds as a component and as a native program (`--features native`). Its `bulkhead.toml` declares the extension.
- `node/`: a node binary with the extension (`greeter-node`). Its test deploys the app twice, granted and not, and calls both, once per build.

```sh
cargo test -p example-greeter-node
```

## Writing an extension

1. Define the package in WIT, in a namespace other than `wasi` or `bulkhead`. Give every function a `result` whose error has an `access-denied` case, so an ungranted app gets an error value, not a trap.
2. Generate host bindings with `bulkhead_host::wasmtime::component::bindgen!`, passing `wasmtime_crate: bulkhead_host::wasmtime` so the bindings use the wasmtime the node links.
3. Implement `bulkhead_host::services::Extension`:
   - `package()`: the package name, which is also the manifest key and the policy grant.
   - `instance(app)`: per-instance state from the app's scope table (`app.scope`) and whether it was granted (`app.granted`). Every instance gets state, including apps that do not declare the extension.
   - `link(linker)`: call the generated `add_to_linker::<StoreData, ExtensionCtx<State>>(linker, extension_ctx::<State>)`.
   - `start(node)`, optional: work on the extension's own schedule (polling devices, serving a port), started once the node's apps are up. It gets the node's `[extensions."<package>"]` table, a way to cast to apps by name, and whether this node runs singletons (on an HA pair, the primary). `crates/bulkhead-modbus` is an extension that does.
   - `call(data, call)`, optional: the same functions for native programs, which import nothing and call through the node. Match on `call.function()` (by convention `interface#function`, `greet#hello`), decode the parameters with `call.args()` as a tuple, call the generated `Host` method on `extension_ctx::<State>(data)`, and return `reply(&result)`. Add `additional_derives: [serde::Serialize, serde::Deserialize]` to `bindgen!` so the WIT types encode. Without it, native programs get `NoSuchFunction`.
4. Implement the generated `Host` traits for `ServiceCtx<'_, State>`. `ctx.state()` is the instance's state; `ctx.app()` and `ctx.pid()` say who is calling. Time a host function spends waiting does not count against the callback deadline; work it does without waiting does.
5. Run a node with it: `bulkhead::node::run(&config, vec![Arc::new(MyService::default())])`.

## Using it from an app

```toml
[appserver.capabilities.extensions."example:greeting"]
salutation = "Howdy"
```

The operator grants it in the node's policy, by package name:

```toml
[apps.greeted]
grant = ["example:greeting"]
```

An app that imports the package without declaring it is refused by `bulkhead bundle` and at deploy. A declared but ungranted app gets `access-denied`. A node built without the extension links deny stubs for the package.

A native program calls through the node with `bulkhead_sdk::extension::call("example:greeting", "greet#hello", &(name,))`, which returns the WIT result decoded from CBOR. `app/src/greeting.rs` wraps that so the app's code is the same in both builds: it generates the bindings on both targets with serde's traits derived, re-exports the generated functions in a component, and defines functions of the same names over `extension::call` natively, turning "the node runs no such extension" into `access-denied` as the deny stubs do. A native program imports nothing, so an undeclared extension is not refused at deploy; the extension sees the app as not granted.

```sh
cargo build -p example-greeter-app --features native --bin example-greeter-app
```
