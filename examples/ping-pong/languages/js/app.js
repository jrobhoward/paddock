// pong in JavaScript (jco componentize), against paddock:runtime/app as jco
// maps it: registers `pong` and answers each CBOR `Ping { n }` with
// `Pong { n: n + 1 }`, as the Rust example's `pong` role does. The Rust
// `ping` app (examples/ping-pong) calls it.
//
// Build with build.sh in the directory above; see its README.

import { register } from "paddock:runtime/process@0.1.0";

const CBOR = "application/cbor";
const encoder = new TextEncoder();

// The one message shape ping-pong uses, {"n": <unsigned>}, as the Rust side's
// ciborium writes it: a map of one entry (0xa1), the text key "n" (0x61 0x6e),
// then an unsigned integer. n is a u64, so a BigInt. A larger app would use a
// CBOR library; this keeps the example free of dependencies.
const KEY = [0xa1, 0x61, 0x6e];

function encodeN(n) {
  if (n < 24n) return Uint8Array.from([...KEY, Number(n)]);
  for (const [info, size] of [[24, 1], [25, 2], [26, 4], [27, 8]]) {
    if (n < 1n << BigInt(8 * size)) {
      const out = new Uint8Array(KEY.length + 1 + size);
      out.set([...KEY, info]);
      for (let i = 0; i < size; i++) {
        out[out.length - 1 - i] = Number((n >> BigInt(8 * i)) & 0xffn);
      }
      return out;
    }
  }
  throw new Error("n does not fit in 64 bits");
}

function decodeN(body) {
  if (body.length < KEY.length + 1 || KEY.some((b, i) => body[i] !== b)) {
    throw new Error('not a CBOR map of one key "n"');
  }
  const head = body[KEY.length];
  const rest = body.subarray(KEY.length + 1);
  if (head >> 5 !== 0) throw new Error("n is not an unsigned integer");
  const info = head & 0x1f;
  if (info > 27) throw new Error("bad length for n");
  const want = info < 24 ? 0 : 1 << (info - 24);
  if (rest.length !== want) throw new Error("bad length for n");
  if (info < 24) return BigInt(info);
  return rest.reduce((n, b) => (n << 8n) | BigInt(b), 0n);
}

function text(s) {
  return { contentType: "text/plain", body: encoder.encode(s) };
}

export function init(_args) {
  try {
    register("pong");
  } catch (e) {
    // A WIT `result` error arrives as a thrown value with the error as its
    // payload; an export's error is a thrown string.
    throw `register pong: ${JSON.stringify(e.payload ?? String(e))}`;
  }
}

export function handleCall(_caller, msg) {
  if (msg.contentType.split(";")[0].trim() !== CBOR) {
    return { tag: "reply", val: text(`expected a Ping: content type is not ${CBOR}`) };
  }
  let n;
  try {
    n = decodeN(msg.body);
  } catch (e) {
    return { tag: "reply", val: text(`expected a Ping: ${e.message}`) };
  }
  return {
    tag: "reply",
    val: { contentType: CBOR, schema: "ping-pong.pong.v1", body: encodeN(n + 1n) },
  };
}

// pong is sent no casts and receives no info.
export function handleCast(_msg) {}
export function handleInfo(_msg) {}
export function terminate(_reason) {}

// pong keeps no state, so an upgrade has nothing to carry.
export function saveSnapshot() {
  return new Uint8Array(0);
}
export function loadSnapshot(_fromVersion, _snapshot) {}
