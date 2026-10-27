"""pong in Python (componentize-py), against the bindings componentize-py
generates from wit/runtime.wit: registers `pong` and answers each CBOR
`Ping { n }` with `Pong { n: n + 1 }`, as the Rust example's `pong` role
does. The Rust `ping` app (examples/ping-pong) calls it.

Build with build.sh in the directory above; see its README.
"""

import wit_world
from componentize_py_types import Err
from wit_world.imports import process
from wit_world.imports.types import (
    CallResult,
    CallResult_Reply,
    Envelope,
    ExitReason,
    Info,
)

CBOR = "application/cbor"

# The one message shape ping-pong uses, {"n": <unsigned>}, as the Rust side's
# ciborium writes it: a map of one entry (0xa1), the text key "n" (0x61 0x6e),
# then an unsigned integer. A larger app would use a CBOR library (cbor2 is
# pure Python); this keeps the example free of dependencies.
KEY = b"\xa1\x61n"


def encode_n(n: int) -> bytes:
    if n < 24:
        return KEY + bytes([n])
    for info, size in ((24, 1), (25, 2), (26, 4), (27, 8)):
        if n < 1 << (8 * size):
            return KEY + bytes([info]) + n.to_bytes(size, "big")
    raise ValueError("n does not fit in 64 bits")


def decode_n(body: bytes) -> int:
    if not body.startswith(KEY) or len(body) < len(KEY) + 1:
        raise ValueError('not a CBOR map of one key "n"')
    head, rest = body[len(KEY)], body[len(KEY) + 1 :]
    if head >> 5 != 0:
        raise ValueError("n is not an unsigned integer")
    info = head & 0x1F
    if info < 24:
        want = 0
    elif info <= 27:
        want = 1 << (info - 24)
    else:
        raise ValueError("bad length for n")
    if len(rest) != want:
        raise ValueError("bad length for n")
    return info if info < 24 else int.from_bytes(rest, "big")


def text(s: str) -> Envelope:
    return Envelope("text/plain", None, s.encode())


class WitWorld(wit_world.WitWorld):
    def init(self, args: bytes) -> None:
        try:
            process.register("pong")
        except Err as e:
            raise Err(f"register pong: {e.value}")

    def handle_call(self, caller: int, msg: Envelope) -> CallResult:
        if msg.content_type.split(";")[0].strip() != CBOR:
            return CallResult_Reply(text(f"expected a Ping: content type is not {CBOR}"))
        try:
            n = decode_n(msg.body)
        except ValueError as e:
            return CallResult_Reply(text(f"expected a Ping: {e}"))
        return CallResult_Reply(Envelope(CBOR, "ping-pong.pong.v1", encode_n(n + 1)))

    # pong is sent no casts and receives no info.
    def handle_cast(self, msg: Envelope) -> None:
        pass

    def handle_info(self, msg: Info) -> None:
        pass

    def terminate(self, reason: ExitReason) -> None:
        pass

    # pong keeps no state, so an upgrade has nothing to carry.
    def save_snapshot(self) -> bytes:
        return b""

    def load_snapshot(self, from_version: str, snapshot: bytes) -> None:
        pass
