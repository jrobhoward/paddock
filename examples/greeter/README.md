# Greeter: a host service from outside Paddock

An extension is a host service built into a node by its operator, serving one WIT package that apps import. This example has four parts:

- `wit/greeting.wit`: the package, `example:greeting`, with `hello(name)` and `count()`.
- `service/`: the extension. It depends on `paddock-host` alone.
- `app/`: an app that calls it, written with `paddock-sdk` plus its own bindings for the package. Its `paddock.toml` declares the extension.
- `node/`: a node binary with the extension (`greeter-node`). Its test deploys the app twice, granted and not, and calls both.

```sh
cargo test -p example-greeter-node
```

## Writing an extension

1. Define the package in WIT, in a namespace other than `wasi` or `paddock`. Give every function a `result` whose error has an `access-denied` case, so an ungranted app gets an error value, not a trap.
2. Generate host bindings with `paddock_host::wasmtime::component::bindgen!`, passing `wasmtime_crate: paddock_host::wasmtime` so the bindings use the wasmtime the node links.
3. Implement `paddock_host::services::Extension`:
   - `package()`: the package name, which is also the manifest key and the policy grant.
   - `instance(app)`: per-instance state from the app's scope table (`app.scope`) and whether it was granted (`app.granted`). Every instance gets state, including apps that do not declare the extension.
   - `link(linker)`: call the generated `add_to_linker::<StoreData, ExtensionCtx<State>>(linker, extension_ctx::<State>)`.
4. Implement the generated `Host` traits for `ServiceCtx<'_, State>`. `ctx.state()` is the instance's state; `ctx.app()` and `ctx.pid()` say who is calling. Time spent in a host function does not count against the callback deadline.
5. Run a node with it: `paddock::node::run(&config, vec![Arc::new(MyService::default())])`.

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

An app that imports the package without declaring it is refused by `paddock bundle` and at deploy. A declared but ungranted app gets `access-denied`. A node built without the extension links deny stubs for the package.
