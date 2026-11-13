# Bulkhead: App message formats

2026-10-29 · Companion to [`protocol.rs`](../crates/bulkhead-native/src/protocol.rs) and [WRITING_APPS.md](WRITING_APPS.md)

The protocol fixes the frames between an app and its node. Several messages that cross them have a body whose shape is Bulkhead's: an HTTP request arrives as a call whose body is a CBOR map, a health check is a call with a reserved schema, and a key change arrives as a host event with a JSON body. This document gives each such format.

The examples are the reference. Each is a fenced block whose info string names its encoding and the example (` ```cbor-hex http-request `, ` ```json kv-changed `). Tests read the blocks from this file and check them against the code that writes or reads the format: `bulkhead-sdk`'s `formats_tests.rs`, `bulkhead-host`'s `http_tests.rs` and `bulkhead-modbus`'s `points_tests.rs`. An SDK in another language can test against the same blocks. CBOR is shown as hex, with its diagnostic notation ([RFC 8949](https://www.rfc-editor.org/rfc/rfc8949.html#section-8)) above it.

## Envelopes

Every message is an envelope (`bulkhead:runtime/types`): a content type, an optional schema and a body of bytes.

- **Content types.** `text/plain` (UTF-8), `application/json` and `application/cbor` are the ones Bulkhead reads. Parameters are allowed and ignored (`application/json; charset=utf-8`).
- **Schemas.** A schema names a body's shape. Names starting `bulkhead/` are Bulkhead's, listed below. An app names its own as it likes; the examples use `<app>.<message>.v<version>` (`ping-pong.ping.v1`).
- **CBOR.** Readers accept any well-formed CBOR, with definite or indefinite lengths. Bulkhead writes definite lengths and the shortest integer encodings (RFC 8949's preferred serialization), so the bytes it writes are the ones shown. Bytes are CBOR byte strings (major type 2), never arrays of integers.
- **Maps.** Readers find map entries by key, accept them in any order, and ignore keys they do not know, so that a field can be added later without breaking older readers. Writers keep the order shown.
- **From the CLI.** `bulkhead call` and `bulkhead cast` send `text/plain` unless `--content-type` says otherwise; with `--cbor` they read the body as JSON, send it as `application/cbor`, and print a CBOR reply as JSON. An app that takes text commands or CBOR maps can be driven from the shell.

## Snapshots

What `save-snapshot` returns and `load-snapshot` receives belongs to the app: the host passes the bytes from the old version to the new one in an upgrade, and reads them only to show them (`bulkhead state`). The Rust SDK writes a CBOR array of two items:

- the state's format version, an unsigned integer that the app raises whenever the state's shape changes;
- the state.

A reader decodes the state when the version is its own, and otherwise migrates from the version given or refuses. `bulkhead state` shows a snapshot in this format as `format: bulkhead-sdk`, with its version, and the state as JSON. An SDK in another language that writes the same array lets state cross languages in an upgrade, when both versions read the same state shape.

```text
[2, {"count": 5, "label": "board"}]
```

```cbor-hex snapshot
8202a265636f756e7405656c6162656c65626f617264
```

## HTTP requests and responses

The node's HTTP ingress sends each request on an app's routes as a call: content type `application/cbor`, schema `bulkhead/http-request`, a CBOR map with these keys, in this order:

| Key | Type | What |
| --- | --- | --- |
| `scheme` | text | `http` or `https` |
| `method` | text | As sent, `GET`, `POST` ... |
| `path` | text | Without the query |
| `query` | text or null | After the `?`, without it; null when there is none |
| `headers` | array of `[name, value]` text pairs | Names in lower case, in the order received; a repeated header appears once per value; a value that is not UTF-8 has its invalid bytes replaced |
| `body` | bytes | Up to the route's `max_body` |
| `key` | text | Only when the route has a `hash_header`: that header's value, for `instances.routing = "hash:key"` |

```text
{"scheme": "http", "method": "POST", "path": "/board/inc", "query": "by=2",
 "headers": [["host", "localhost:8080"], ["content-type", "text/plain"]],
 "body": h'6869', "key": "tenant-7"}
```

```cbor-hex http-request
a766736368656d656468747470666d6574686f6464504f535464706174686a2f
626f6172642f696e636571756572796462793d32676865616465727382826468
6f73746e6c6f63616c686f73743a38303830826c636f6e74656e742d74797065
6a746578742f706c61696e64626f6479426869636b65796874656e616e742d37
```

The app answers from `handle-call`, now or later with `process.reply`. A reply with schema `bulkhead/http-response` is a CBOR map:

| Key | Type | What |
| --- | --- | --- |
| `status` | unsigned | The status code; 200 when missing |
| `headers` | array of `[name, value]` text pairs | Optional |
| `body` | bytes | Optional, empty when missing |

```text
{"status": 201, "headers": [["content-type", "text/plain"]], "body": h'6f6b'}
```

```cbor-hex http-response
a36673746174757318c9676865616465727381826c636f6e74656e742d747970
656a746578742f706c61696e64626f6479426f6b
```

A reply with any other schema is sent as `200`, with the envelope's content type as `content-type` and its body. A reply that has the schema but is not such a map is answered `502`, as is one larger than the route's `max_response`. MANIFEST.md has the route settings, and `crates/bulkhead-host/src/http.rs` the statuses for each failure.

## Health checks

An app whose manifest has a `health` section is called every `interval_ms` on each instance: content type `text/plain`, schema `bulkhead/health`, body `health`. A reply whose body is exactly `ok` is healthy, whatever its content type and schema. Any other body is a failure, and the node logs it as the reason; so are an error and no reply within the interval. The Rust SDK replies with schema `bulkhead/health` and content type `text/plain`.

## Batches

Many small items to the same instance travel as one message (`bulkhead_sdk::batch`): content type `application/cbor`, schema `bulkhead/batch`, a CBOR array of the items. Each item is what the sender's item type encodes to; the Rust SDK encodes a struct as a map from field names to values.

```text
[{"device": "pump-1", "value": 17}, {"device": "pump-2", "value": 3}]
```

```cbor-hex batch
82a2666465766963656670756d702d316576616c756511a26664657669636566
70756d702d326576616c756503
```

## Hash routing

An app with `instances.routing = "hash:<field>"` has each call and cast sent to the instance its key picks. The key is the top-level `<field>` of the body, which must be a map in an `application/json` or `application/cbor` envelope:

- a text string gives its UTF-8 bytes;
- an integer gives its decimal digits, so `17` and `"17"` route alike;
- a CBOR byte string gives its bytes.

A body of another content type, one that is not a map, one without the field, or one whose field has another type goes to the next instance in turn. The same key always picks the same instance while the pool's size is unchanged, on every node. HTTP requests carry their route's `hash_header` in `key` (above), for `hash:key`.

## Host events

The host tells an instance about events with `handle-info`'s `host-event(name, envelope)`. An app ignores names it does not know; new names may be added without a new version of the app world.

| Name | Envelope | When |
| --- | --- | --- |
| `log-level` | `text/plain`: `trace`, `debug`, `info`, `warn`, `error` or `critical` | After `init` and whenever the app's effective level changes (`bulkhead log-level`); only to instances granted logging. The host drops records below it anyway; an SDK uses it to skip formatting them. |
| `failover-role` | `text/plain`: `primary`, `standby` or `down` | After `init` and on every change of the node's role in an HA pair; only to apps with `placement.mode = "hot-standby"`. |
| `kv-changed` | `application/json`, below | A key a watch covers changed (`bulkhead:watch`). Changes made while a notice waits are folded into it: one notice per key, carrying the latest revision when it was taken. |
| `io-ready` | `application/json`, below | A pollable handed to `bulkhead:io/ready.notify` became ready. |
| `routes` | `application/json`, below | After `init` and whenever the node's HTTP route table changes (a deploy, an upgrade, a stop); only to a driver app whose node config asks for it (`[drivers.<app>] routes = true`, ROADMAP DA1). |

The JSON objects' keys come in any order.

```json kv-changed
{"watch": 1, "bucket": "board", "key": "count", "revision": 7}
```

`watch` is the id `watch` returned, `revision` the key's revision; both are unsigned 64-bit integers.

```json io-ready
{"tag": 3}
```

`tag` is the unsigned 64-bit tag passed to `notify`.

```json routes
{"routes": [{"app": "shop", "host": null, "path_prefix": "/shop", "hash_header": "x-customer", "max_body": 1048576, "timeout_ms": 30000}]}
```

`routes` is the whole table, every app's routes, ordered by app: the routes a node's own ingress serves, so a driver serving HTTP routes and limits requests as the ingress does and calls the app by name (`call_app`). `host` and `hash_header` are `null` when unset; `max_body` is bytes and `timeout_ms` milliseconds.

## Modbus updates

The Modbus extension (`bulkhead-modbus`, off by default) casts to the app its configuration names: content type `application/cbor`, no schema, a CBOR map:

| Key | Type | What |
| --- | --- | --- |
| `device` | text | The device's name (the server's `name` for `written`): the routing key for `hash:device` |
| `kind` | text | `points` (a scan's changes), `offline` (the device stopped answering) or `written` (a master wrote to the node's server) |
| `full` | bool | Whether `points` holds every polled point, not only the changes; true for the first scan after an `offline` |
| `points` | array of maps | Each `{table, address, value}`: `table` is `holding`, `input`, `coil` or `discrete`; `address` and `value` are unsigned 16-bit integers, with 0 or 1 for coils and discrete inputs |
| `error` | text | Only with `offline`: why |

```text
{"device": "pump-1", "kind": "points", "full": false,
 "points": [{"table": "holding", "address": 3, "value": 17}]}
```

```cbor-hex modbus-points
a4666465766963656670756d702d31646b696e6466706f696e74736466756c6c
f466706f696e747381a3657461626c6567686f6c64696e676761646472657373
036576616c756511
```

```text
{"device": "pump-1", "kind": "offline", "full": false, "points": [],
 "error": "connection refused"}
```

```cbor-hex modbus-offline
a5666465766963656670756d702d31646b696e64676f66666c696e656466756c
6cf466706f696e747380656572726f7272636f6e6e656374696f6e2072656675
736564
```

## Not covered here

- The frames between a node and its process (`crates/bulkhead-native/src/protocol.rs`, and the hot frames' fixed layouts in `hot.rs`'s module docs), the channel they travel over (`channel.rs`) and published buckets' snapshots (`snapshot.rs`, its layout in the module docs).
- Bodies an app defines for its own messages.
