// Spike S23: a Paddock app in JavaScript (jco componentize, StarlingMonkey),
// against paddock:runtime/app as jco maps it (types/ from `jco guest-types`).
// The same text protocol and misbehaving commands as S21's Go app.
//
// Calls (text/plain): inc, get, after <ms> (a timer that incs through
// handle-info), self (a call to its own registered name, which must fail),
// panic, spin, alloc <mib>.

import {
  register,
  whereis,
  call,
  sendAfter,
} from "paddock:runtime/process@0.1.0";

const VERSION = "js-1";
const encoder = new TextEncoder();
const decoder = new TextDecoder();

let count = 0n;
const ballast = [];

function text(s) {
  return { contentType: "text/plain", body: encoder.encode(s) };
}

function command(cmd) {
  const [word, arg = ""] = cmd.trim().split(" ", 2);
  switch (word) {
    case "inc":
      count += 1n;
      return String(count);
    case "get":
      return `${VERSION} count=${count}`;
    case "after":
      sendAfter(Number(arg) || 0, text("inc"));
      return "ok";
    case "self": {
      const pid = whereis("jscounter");
      if (pid === undefined) return "not registered";
      try {
        const reply = call(pid, text("get"), 1000);
        return "call replied: " + decoder.decode(reply.body);
      } catch (e) {
        return "call failed: " + JSON.stringify(e.payload ?? String(e));
      }
    }
    case "panic":
      throw new Error("asked to panic");
    case "spin":
      for (;;) {}
    case "alloc":
      for (let i = 0; i < (Number(arg) || 0); i++) {
        ballast.push(new Uint8Array(1 << 20).fill(1));
      }
      return `held ${ballast.length} MiB`;
  }
  return "commands: inc, get, after <ms>, self, panic, spin, alloc <mib>";
}

export function init(args) {
  const a = args.length ? JSON.parse(decoder.decode(args)) : {};
  if (a.register) {
    try {
      register(a.register);
    } catch (e) {
      throw `register ${a.register}: ${JSON.stringify(e.payload ?? String(e))}`;
    }
  }
}

export function handleCall(_caller, msg) {
  return { tag: "reply", val: text(command(decoder.decode(msg.body))) };
}

export function handleCast(msg) {
  command(decoder.decode(msg.body));
}

export function handleInfo(msg) {
  if (msg.tag === "timer") command(decoder.decode(msg.val[1].body));
}

export function terminate(_reason) {}

export function saveSnapshot() {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, count, true);
  return b;
}

export function loadSnapshot(fromVersion, snapshot) {
  if (snapshot.length !== 8) throw `snapshot from ${fromVersion}: want 8 bytes`;
  count = new DataView(snapshot.buffer, snapshot.byteOffset, 8).getBigUint64(0, true);
}
