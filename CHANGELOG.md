# Changelog

All notable changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## Unreleased

### Added

- `wit/runtime.wit`: the `paddock:runtime@0.1.0` app contract (`types`,
  `process`, and the `app` world).
- Tests that enforce the contract's rules: synchronous functions only, no
  `stream`/`future`, no `wasi:io` types, and an `access-denied` error on every
  capability function.
- `paddock-fixture-echo`, the smallest app implementing the `app` world.
- CI: tests on Linux, macOS and Windows, guest component checks, MSRV 1.96.0,
  cargo-deny, fmt and docs, and a weekly run against fresh dependencies.
- `paddock-manifest`: the full app manifest model (every section, including
  fields for later phases), with defaults, path-aware parse errors, validation
  that reports every broken rule, and a list of fields Phase 1 does not enforce.
- `paddock-bundle`: opens `.rbp` 1.2 bundles, verifies signatures against
  operator-trusted keys only, checks checksums and the component's world, and
  reads the `appserver` section.
- Deploy-time import inspection: a component importing an interface its
  manifest does not declare, or that no host provides, is refused.
- Operator policy (TOML) and the effective grant: imports ∩ manifest ∩ policy,
  with ungranted capabilities marked for deny stubs.
- `paddock-host`: the instance runtime. A pooling-allocator engine sized from
  node config (standard or dense memory profile), an epoch ticker, pids with
  incarnations, a process table, and instances that run `init`, handle calls
  and casts one at a time under a per-callback deadline, and run `terminate`
  on a graceful stop. Traps, memory and table limits, deadlines and kills end
  an instance with an exit reason; a full mailbox fails casts and calls with
  `overloaded`.
- `paddock-fixture-chaos`: a guest that traps, spins, allocates or fails on
  request, for exit-path tests.
- Messaging: guest `cast`, `call` (with timeout, `noproc` for dead or stale
  pids, the target's exit reason if it dies mid-call) and `reply` for deferred
  answers; `send-after` and `cancel-timer` timers delivered to `handle-info`;
  a node-local registry (`register` limited to the manifest's
  `registered_names`, `whereis`, names released on exit). `Runtime` gains
  `whereis`, `call` and `cast`.
- `message_max_bytes` is enforced; an oversized body fails with the new
  `too-large` call error. Casts dropped for a full mailbox are counted
  (`InstanceHandle::dropped_messages`).
- Links, monitors and exit signals: `link`, `unlink`, `trap-exits`,
  `monitor`, `demonitor` and `exit` follow OTP's propagation rules. Exit and
  down notifications never drop.
- Host-native supervisors (`Runtime::start_supervisor`): `one_for_one`,
  `one_for_all`, `rest_for_one` and `simple_one_for_one`; permanent,
  transient and temporary children; restart intensity with escalation;
  backoff with jitter; nested supervisors.
- Apps (`Runtime::start_apps`): dependency order (with version checks and
  cycle detection), a root supervisor per app built from its manifest,
  status (`running`, `failed` with the escalation reason, `stopped`), stop in
  reverse order, and instance pools with round-robin and JSON/CBOR hash
  routing. `whereis` on an app's name returns a pool member.
- Host services: `wasi:logging` (level cap, per-app rate limit, tagged
  records), stdout/stderr captured as tagged logs, `wasi:config`,
  `wasi:keyvalue` (memory and redb disk buckets, per-app namespaces, byte
  quotas, `increment`, batch calls) and `wasi:filesystem` preopens (`/priv`
  read-only, volumes read-write). Ungranted capabilities are denied per
  instance; interfaces no host implements get typed deny stubs. Vendored WIT
  for the draft interfaces lives in `wit/vendor`.
- `RuntimeConfig::data_dir` enables disk buckets. `InstanceSpec::new`,
  `with_granted` and `resolve_preopens`.
- Hot upgrade: `InstanceHandle::upgrade` swaps an instance's component
  between callbacks through `save-snapshot`/`load-snapshot`, keeping its pid,
  links, monitors, names, timers and waiting callers; any failure after the
  snapshot keeps the old version. `Apps::upgrade` rolls an app out in
  `snapshot` or `restart` mode, in batches, stopping and reverting on a failed
  instance or too many restarts; `Apps::rollback` returns to the previous
  version. `validate_upgrade` refuses other apps, the same version, a world
  change, versions outside `from_versions`, and unapproved new capabilities.
- Supervisors can replace a child's spec, with or without restarting it
  (`replace_child`, `replace_template`), and count their restarts.
- `paddock-node`: the node daemon. TOML node config (trusted keys, policy,
  volumes, per-app config overrides), a deploy store with bundles, `priv/`
  files and desired state, restore on restart, an audit log, and the admin API
  (JSON over HTTP on a 0600 Unix socket): deploy, list, status, stop, start,
  remove, kill, upgrade, rollback, logs, ps and call.
- `paddock` CLI over the admin API, with text and `--json` output.
- `paddock-api`: the admin API's request and response types and a client.
- Bundles' `priv/` files are verified against the signed manifest's checksums
  and extracted for `/priv`; an unlisted or altered file refuses the deploy.
- `AppDeployment` carries its `InstanceSpec`; `Apps::add` and `Apps::remove`
  change a running set; instances report app, version and queue depth;
  `Runtime::instances`; `Pid` parses from text.
- `wit/runtime.wit`: `info` gains a reserved `host-event(tuple<string,
  envelope>)` case, so later host events reach `@0.1.0` guests.
- `paddock-sdk`: write apps in Rust with the `App` trait, `Ctx` (every
  `process` function), typed envelopes (CBOR, JSON, text), logging macros, and
  `paddock_sdk::app!`. `#[derive(Snapshot)]` (`paddock-sdk-macros`) gives
  versioned snapshot state with a `migrate` hook.
- Examples written with the SDK: `counter` (v1, v2, broken v3), `ping-pong`
  (typed CBOR calls, registry, monitor) and `chaos`, each with a
  `paddock.toml`.
- `paddock bundle` builds a signed `.rbp` from a component and `paddock.toml`
  (refusing undeclared imports, adding `--priv` files), and `paddock keygen`
  creates minisign keys.
- `paddock state <pid>` (`GET /v1/instances/{pid}/state`): an instance's
  state through `save-snapshot`, shown as JSON for SDK snapshots, JSON or text
  bodies, or hex (`InstanceHandle::snapshot` in `paddock-host`).
- `paddock log-level <app> <level|reset>` (`POST /v1/apps/{app}/log-level`):
  change an app's log level at run time, audited (`Runtime::set_log_level`).
- `paddock cast <app> <body>` (`POST /v1/apps/{app}/cast`), and a `DROPPED`
  column in `paddock ps` (`InstanceInfo::dropped`): casts lost to a full
  mailbox.
- A node holds an exclusive lock on `<data_dir>/node.lock`; a second node on
  the same data directory is refused.
- `crates/paddock-node/tests/exit_criteria.rs`: every Phase 1 exit criterion
  as a test against a running node.
- `docs/BASELINE.md` and `paddock-host`'s `baseline` example: call latency,
  cast throughput, instantiate time and idle memory per instance.
- `fuzz/`: cargo-fuzz targets for bundle opening, manifest parsing, small
  parsers, hash-routing keys and sequences of process operations on live
  instances, run nightly in CI.
- `docs/GETTING_STARTED.md`, `docs/WRITING_APPS.md` and `docs/MANIFEST.md`.
- `docs/ROADMAP.md`, the single plan for remaining work, replacing
  `docs/PHASE1_PLAN.md`.

### Changed

- Crates consolidated from nine to five publishable ones. `paddock-manifest`
  is now the `manifest` module of `paddock-bundle`. `paddock-node`,
  `paddock-api` and `paddock-cli` are now the `paddock` crate: the node runs
  as `paddock node --config node.toml` (was `paddock-node`), and the admin
  API is `paddock::api`. Building `paddock` without its default `node`
  feature gives the CLI and bundle tooling without wasmtime.
- `rustbridge-bundle` is a git dependency on rustbridge's `experiment/wasm`
  branch instead of a sibling checkout, so CI can build the workspace.
