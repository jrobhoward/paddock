"""Spike S22: a Paddock app in Python (componentize-py), against the raw
bindings componentize-py generates from wit/runtime.wit. The same text
protocol and misbehaving commands as S21's Go app.

Calls (text/plain): inc, get, after <ms> (a timer that incs through
handle-info), self (a call to its own registered name, which must fail),
panic, spin, alloc <mib>.
"""

import json
import struct

import wit_world
from componentize_py_types import Err
from wit_world.imports import process
from wit_world.imports.types import (
    CallResult,
    CallResult_Reply,
    Envelope,
    ExitReason,
    Info,
    Info_Timer,
)

VERSION = "py-1"

count = 0
ballast: list[bytearray] = []


def text(s: str) -> Envelope:
    return Envelope("text/plain", None, s.encode())


def command(cmd: str) -> str:
    global count
    word, _, arg = cmd.strip().partition(" ")
    if word == "inc":
        count += 1
        return str(count)
    if word == "get":
        return f"{VERSION} count={count}"
    if word == "after":
        process.send_after(int(arg or 0), text("inc"))
        return "ok"
    if word == "self":
        pid = process.whereis("pycounter")
        if pid is None:
            return "not registered"
        try:
            reply = process.call(pid, text("get"), 1000)
        except Err as e:
            return f"call failed: {e.value}"
        return "call replied: " + reply.body.decode()
    if word == "panic":
        raise RuntimeError("asked to panic")
    if word == "spin":
        while True:
            pass
    if word == "alloc":
        for _ in range(int(arg or 0)):
            ballast.append(bytearray(b"\x01" * (1 << 20)))
        return f"held {len(ballast)} MiB"
    return "commands: inc, get, after <ms>, self, panic, spin, alloc <mib>"


class WitWorld(wit_world.WitWorld):
    def init(self, args: bytes) -> None:
        a = json.loads(args) if args else {}
        name = a.get("register")
        if name:
            try:
                process.register(name)
            except Err as e:
                raise Err(f"register {name}: {e.value}")

    def handle_call(self, caller: int, msg: Envelope) -> CallResult:
        return CallResult_Reply(text(command(msg.body.decode())))

    def handle_cast(self, msg: Envelope) -> None:
        command(msg.body.decode())

    def handle_info(self, msg: Info) -> None:
        if isinstance(msg, Info_Timer):
            command(msg.value[1].body.decode())

    def terminate(self, reason: ExitReason) -> None:
        pass

    def save_snapshot(self) -> bytes:
        return struct.pack("<Q", count)

    def load_snapshot(self, from_version: str, snapshot: bytes) -> None:
        global count
        if len(snapshot) != 8:
            raise Err(f"snapshot from {from_version}: want 8 bytes")
        (count,) = struct.unpack("<Q", snapshot)
