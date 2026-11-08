# Greeter: a host service from outside Bulkhead

An extension is a host service built into a node by its operator, serving one package that apps call through the node. This example has three parts:

- `service/`: the extension, the `example:greeting` package, with `hello(name)` and `count()`. It depends on `bulkhead-host` alone.
- `app/`: an app that calls it, written with `bulkhead-sdk` plus a client module for the package (`src/greeting.rs`). Its `bulkhead.toml` declares the extension.
- `node/`: a node binary with the extension (`greeter-node`). Its test deploys the app twice, granted and not, and calls both.

```sh
cargo test -p example-greeter-node
```

## Writing an extension

1. Name the package in a namespace other than `bulkhead`. Give every function a `Result` whose error has an `AccessDenied` case, so an ungranted app gets an error value, not a crash.
2. Implement `bulkhead_host::services::Extension`:
   - `package()`: the package name, which is also the manifest key and the policy grant.
   - `instance(app)`: per-instance state from the app's scope table (`app.scope`) and whether it was granted (`app.granted`). Every instance gets state, including apps that do not declare the extension.
   - `start(node)`, optional: work on the extension's own schedule (polling devices, serving a port), started once the node's apps are up. It gets the node's `[extensions."<package>"]` table, a way to cast to apps by name, and whether this node runs singletons (on an HA pair, the primary). `crates/bulkhead-modbus` is an extension that does.
   - `call(data, call)`: the package's functions. Match on `call.function()` (by convention `interface#function`, `greet#hello`), decode the parameters with `call.args()` as a tuple, take the instance's state from `extension_ctx::<State>(data)`, and return `reply(&result)`. `ctx.state()` is the instance's state; `ctx.app()` and `ctx.pid()` say who is calling. Time a function spends waiting does not count against the callback deadline; work it does without waiting does.
3. Run a node with it: `bulkhead::node::run(&config, vec![Arc::new(MyService::default())])`.

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

A declared but ungranted app gets `AccessDenied`, as does an app on a node built without the extension.

An app calls through the node with `bulkhead_sdk::extension::call("example:greeting", "greet#hello", &(name,))`, which returns the result decoded from CBOR. `app/src/greeting.rs` gives each function its name and types.
