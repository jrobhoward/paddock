// pong in Go (TinyGo), against the bindings wit-bindgen-go generates from
// world.wit: registers `pong` and answers each CBOR `Ping { n }` with
// `Pong { n: n + 1 }`, as the Rust example's `pong` role does. The Rust
// `ping` app (examples/ping-pong) calls it.
//
// Build with build.sh in the directory above; see its README.
package main

import (
	"strings"

	"example.com/pong/gen/paddock/runtime/process"
	"example.com/pong/gen/paddock/runtime/types"
	"example.com/pong/gen/pong/go/app"
	"go.bytecodealliance.org/cm"
)

const cbor = "application/cbor"

type result = cm.Result[string, struct{}, string]

func text(s string) types.Envelope {
	return types.Envelope{ContentType: "text/plain", Body: cm.ToList([]byte(s))}
}

func init() {
	app.Exports.Init = func(args cm.List[uint8]) result {
		freeList(args)
		r := process.Register("pong")
		if err := r.Err(); err != nil {
			return cm.Err[result]("register pong: " + err.String())
		}
		return cm.OK[result](struct{}{})
	}

	app.Exports.HandleCall = func(_ app.CallRef, msg app.Envelope) types.CallResult {
		// Read the message before freeing it.
		mediaType, _, _ := strings.Cut(msg.ContentType, ";")
		isCBOR := strings.TrimSpace(mediaType) == cbor
		n, err := decodeN(msg.Body.Slice())
		freeEnvelope(msg)

		if !isCBOR {
			return types.CallResultReply(text("expected a Ping: content type is not " + cbor))
		}
		if err != nil {
			return types.CallResultReply(text("expected a Ping: " + err.Error()))
		}
		return types.CallResultReply(types.Envelope{
			ContentType: cbor,
			Schema:      cm.Some("ping-pong.pong.v1"),
			Body:        cm.ToList(encodeN(n + 1)),
		})
	}

	// pong is sent no casts and receives no info; free them if it is.
	app.Exports.HandleCast = func(msg app.Envelope) { freeEnvelope(msg) }
	app.Exports.HandleInfo = func(info types.Info) { freeInfo(info) }
	app.Exports.Terminate = func(reason types.ExitReason) { freeExitReason(reason) }

	// pong keeps no state, so an upgrade has nothing to carry.
	app.Exports.SaveSnapshot = func() cm.List[uint8] { return cm.List[uint8]{} }
	app.Exports.LoadSnapshot = func(from string, snapshot cm.List[uint8]) result {
		freeString(from)
		freeList(snapshot)
		return cm.OK[result](struct{}{})
	}
}

// TinyGo requires a main; the host calls the exports above instead.
func main() {}
