// Package paddock is the Go SDK for Paddock apps, built with TinyGo.
//
// An app is a gen_server-style set of callbacks over its state. The host
// calls one callback at a time. Only Init and HandleCall are required; an
// app implements [CastHandler], [InfoHandler], [Terminator],
// [HealthChecker] and [Snapshotter] for the rest.
//
//	type Greeter struct{ greeted int }
//
//	func (g *Greeter) HandleCall(ctx *paddock.Ctx, call paddock.CallRef, msg paddock.Envelope) paddock.Reply {
//		g.greeted++
//		name, _ := msg.AsText()
//		return paddock.Now(paddock.Text("hello, " + name))
//	}
//
//	func init() {
//		paddock.Export(paddock.Def{
//			Init: func(*paddock.Ctx, []byte) (paddock.App, error) { return &Greeter{}, nil },
//		})
//	}
//
//	func main() {}
//
// Build with the SDK's build.sh, which passes TinyGo the SDK's WIT world
// and `-scheduler=none`. The SDK copies everything the host passes and
// frees the original, so an app keeps what it is given.
package paddock

import (
	"github.com/jrobhoward/paddock/sdks/go/internal/abi"
	"github.com/jrobhoward/paddock/sdks/go/internal/gen/paddock/go-sdk/app"
	"github.com/jrobhoward/paddock/sdks/go/internal/gen/paddock/runtime/types"
	"github.com/jrobhoward/paddock/sdks/go/internal/loglevel"
	"go.bytecodealliance.org/cm"
)

// App handles calls. It is the state of one instance.
type App interface {
	HandleCall(ctx *Ctx, call CallRef, msg Envelope) Reply
}

// CastHandler handles casts. Without it, casts are dropped.
type CastHandler interface {
	HandleCast(ctx *Ctx, msg Envelope)
}

// InfoHandler handles link exits, monitor downs, timers and host events.
type InfoHandler interface {
	HandleInfo(ctx *Ctx, info Info)
}

// Terminator cleans up before a graceful stop. Not called on kills or
// traps.
type Terminator interface {
	Terminate(ctx *Ctx, reason ExitReason)
}

// HealthChecker says whether the instance is healthy, for the host's
// health checks: an app whose manifest has a `health` section is asked
// every interval_ms, and after failure_threshold failures in a row the
// instance is killed as `crashed: unhealthy`. Without it, an instance is
// healthy while it answers.
type HealthChecker interface {
	// Health returns nil when healthy, or why not; the host logs it.
	Health(ctx *Ctx) error
}

// Snapshotter writes the state for a snapshot upgrade; [Def.LoadSnapshot]
// reads it in the new version. Without it, the snapshot is empty.
type Snapshotter interface {
	// SaveSnapshot must not change the state: if the upgrade fails, this
	// instance keeps running. An error traps, and the supervisor restarts
	// the old version.
	SaveSnapshot() ([]byte, error)
}

// Reply is what HandleCall does with the call: answer [Now], or [Later]
// with [Ctx.Reply].
type Reply struct {
	msg   Envelope
	later bool
}

// Now answers the call with msg.
func Now(msg Envelope) Reply { return Reply{msg: msg} }

// Later answers the call later, with [Ctx.Reply].
func Later() Reply { return Reply{later: true} }

// Def is how an app starts.
type Def struct {
	// Init builds the state. args is the manifest's lifecycle.start_args
	// as JSON, or empty. An error fails the start; the supervisor counts
	// it as a restart.
	Init func(ctx *Ctx, args []byte) (App, error)
	// LoadSnapshot builds the state from a snapshot written by version
	// fromVersion of this app. An error refuses it, and the host keeps the
	// old version running. Nil refuses every snapshot upgrade (restart
	// mode still works).
	LoadSnapshot func(ctx *Ctx, fromVersion string, snapshot []byte) (App, error)
}

// The schema of the host's health checks and their replies; the SDK
// answers them from HealthChecker, so HandleCall never sees them.
const healthSchema = "paddock/health"

// The host event that carries the effective log level, handled by the SDK.
const levelEvent = "log-level"

var (
	def   Def
	state App
)

type result = cm.Result[string, struct{}, string]

func current() App {
	if state == nil {
		panic("paddock: callback before a successful init")
	}
	return state
}

func errText(err error) string {
	if err == nil {
		return "failed"
	}
	return err.Error()
}

// Export makes def the component's implementation of the app world. Call it
// once, from an init function.
func Export(d Def) {
	def = d
	app.Exports.Init = func(args cm.List[uint8]) result {
		a, err := def.Init(&Ctx{}, abi.Bytes(args))
		if err != nil {
			return cm.Err[result](errText(err))
		}
		state = a
		return cm.OK[result](struct{}{})
	}
	app.Exports.HandleCall = handleCall
	app.Exports.HandleCast = func(msg types.Envelope) {
		m := takeEnvelope(msg)
		if h, ok := current().(CastHandler); ok {
			h.HandleCast(&Ctx{}, m)
		}
	}
	app.Exports.HandleInfo = func(msg types.Info) {
		info := infoFromHost(msg)
		if ev, ok := info.(EventInfo); ok && ev.Name == levelEvent {
			loglevel.Set(ev.Msg.Body)
			return
		}
		if h, ok := current().(InfoHandler); ok {
			h.HandleInfo(&Ctx{}, info)
		}
	}
	app.Exports.Terminate = func(reason types.ExitReason) {
		r := exitFromHost(reason)
		if h, ok := current().(Terminator); ok {
			h.Terminate(&Ctx{}, r)
		}
	}
	app.Exports.SaveSnapshot = func() cm.List[uint8] {
		s, ok := current().(Snapshotter)
		if !ok {
			return cm.List[uint8]{}
		}
		bytes, err := s.SaveSnapshot()
		if err != nil {
			panic("paddock: save snapshot failed: " + err.Error())
		}
		return cm.ToList(bytes)
	}
	app.Exports.LoadSnapshot = func(from string, snapshot cm.List[uint8]) result {
		fromVersion, bytes := abi.String(from), abi.Bytes(snapshot)
		if def.LoadSnapshot == nil {
			return cm.Err[result]("this version does not load snapshots (from " + fromVersion + "); upgrade in restart mode")
		}
		a, err := def.LoadSnapshot(&Ctx{}, fromVersion, bytes)
		if err != nil {
			return cm.Err[result](errText(err))
		}
		state = a
		return cm.OK[result](struct{}{})
	}
}

func handleCall(caller types.CallRef, msg types.Envelope) types.CallResult {
	m := takeEnvelope(msg)
	if m.Schema == healthSchema {
		body := "ok"
		if h, ok := current().(HealthChecker); ok {
			if err := h.Health(&Ctx{}); err != nil {
				body = errText(err)
			}
		}
		return types.CallResultReply(Text(body).WithSchema(healthSchema).host())
	}
	reply := current().HandleCall(&Ctx{}, CallRef(caller), m)
	if reply.later {
		return types.CallResultNoreply()
	}
	return types.CallResultReply(reply.msg.host())
}
