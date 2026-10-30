package paddock

import (
	"fmt"

	"github.com/jrobhoward/paddock/sdks/go/internal/abi"
	"github.com/jrobhoward/paddock/sdks/go/internal/gen/paddock/runtime/types"
)

// Pid identifies one instance: {node, id, incarnation}. A restart keeps the
// id and bumps the incarnation, so an old pid never reaches the new
// instance. The id's high 24 bits are the creation of the node's start that
// made it, and its low 40 a serial number.
type Pid struct {
	Node        uint32
	ID          uint64
	Incarnation uint32
}

// String writes the pid as the host does: `<node.serial.incarnation@creation>`.
func (p Pid) String() string {
	return fmt.Sprintf("<%d.%d.%d@%d>", p.Node, p.ID&(1<<40-1), p.Incarnation, p.ID>>40)
}

func pidFromHost(p types.Pid) Pid {
	return Pid{Node: p.Node, ID: p.ID, Incarnation: p.Incarnation}
}

func (p Pid) host() types.Pid {
	return types.Pid{Node: p.Node, ID: p.ID, Incarnation: p.Incarnation}
}

// CallRef is a call awaiting a reply. Keep it to answer later with
// [Ctx.Reply]; it is a number, so it can go in a snapshot.
type CallRef uint64

// MonitorRef is a monitor, from [Ctx.Monitor].
type MonitorRef uint64

// TimerRef is a timer, from [Ctx.SendAfter].
type TimerRef uint64

// ExitKind is why an instance stopped.
type ExitKind uint8

const (
	// Normal: without error.
	Normal ExitKind = iota
	// Shutdown: stopped gracefully by its supervisor or an operator.
	Shutdown
	// Killed: stopped without terminate.
	Killed
	// Noproc: the pid does not exist.
	Noproc
	// Noconnection: its node became unreachable.
	Noconnection
	// Timeout: a callback exceeded its deadline.
	Timeout
	// Crashed: it trapped or exceeded a limit; [ExitReason.Detail] says why.
	Crashed
)

// ExitReason is why an instance stopped.
type ExitReason struct {
	Kind ExitKind
	// Detail is the text of a Crashed reason, and empty otherwise.
	Detail string
}

// CrashedWith is the reason Crashed with this text.
func CrashedWith(why string) ExitReason {
	return ExitReason{Kind: Crashed, Detail: why}
}

var exitNames = [...]string{"normal", "shutdown", "killed", "noproc", "noconnection", "timeout", "crashed"}

// String is `normal`, `shutdown` ... or `crashed: <detail>`.
func (r ExitReason) String() string {
	if r.Kind == Crashed {
		return "crashed: " + r.Detail
	}
	if int(r.Kind) < len(exitNames) {
		return exitNames[r.Kind]
	}
	return "unknown"
}

// exitFromHost copies r and frees its text.
func exitFromHost(r types.ExitReason) ExitReason {
	switch {
	case r.Normal():
		return ExitReason{Kind: Normal}
	case r.Shutdown():
		return ExitReason{Kind: Shutdown}
	case r.Killed():
		return ExitReason{Kind: Killed}
	case r.Noproc():
		return ExitReason{Kind: Noproc}
	case r.Noconnection():
		return ExitReason{Kind: Noconnection}
	case r.Timeout():
		return ExitReason{Kind: Timeout}
	default:
		return CrashedWith(abi.TakeCrashed(&r))
	}
}

func (r ExitReason) host() types.ExitReason {
	switch r.Kind {
	case Normal:
		return types.ExitReasonNormal()
	case Shutdown:
		return types.ExitReasonShutdown()
	case Killed:
		return types.ExitReasonKilled()
	case Noproc:
		return types.ExitReasonNoproc()
	case Noconnection:
		return types.ExitReasonNoconnection()
	case Timeout:
		return types.ExitReasonTimeout()
	default:
		return types.ExitReasonCrashed(r.Detail)
	}
}

// CallErrorKind is why a call failed.
type CallErrorKind uint8

const (
	// CallNoproc: no instance has that pid.
	CallNoproc CallErrorKind = iota
	// CallNoconnection: its node is unreachable.
	CallNoconnection
	// CallTimeout: no reply in time.
	CallTimeout
	// CallOverloaded: its mailbox is full.
	CallOverloaded
	// CallTooLarge: the body is over its message_max_bytes.
	CallTooLarge
	// CallAccessDenied: its messaging ACL refused the call.
	CallAccessDenied
	// CallExited: it exited before replying; [CallError.Reason] says why.
	CallExited
)

// CallError is why a call failed.
type CallError struct {
	Kind CallErrorKind
	// Reason is why the target exited, for CallExited.
	Reason ExitReason
}

var callErrorNames = [...]string{"noproc", "noconnection", "timeout", "overloaded", "too large", "access denied"}

func (e *CallError) Error() string {
	if e.Kind == CallExited {
		return "exited: " + e.Reason.String()
	}
	if int(e.Kind) < len(callErrorNames) {
		return callErrorNames[e.Kind]
	}
	return "unknown call error"
}

func callErrorFromHost(e types.CallError) *CallError {
	switch {
	case e.Noproc():
		return &CallError{Kind: CallNoproc}
	case e.Noconnection():
		return &CallError{Kind: CallNoconnection}
	case e.Timeout():
		return &CallError{Kind: CallTimeout}
	case e.Overloaded():
		return &CallError{Kind: CallOverloaded}
	case e.TooLarge():
		return &CallError{Kind: CallTooLarge}
	case e.AccessDenied():
		return &CallError{Kind: CallAccessDenied}
	default:
		return &CallError{Kind: CallExited, Reason: exitFromHost(*e.Exited())}
	}
}

// RegisterError is why a name could not be registered.
type RegisterError struct {
	// Taken is false when the manifest's registered_names does not list
	// the name, and true when another instance holds it.
	Taken bool
	// Holder is the instance holding the name, when Taken.
	Holder Pid
}

func (e *RegisterError) Error() string {
	if e.Taken {
		return "name held by " + e.Holder.String()
	}
	return "name not listed in the manifest"
}

// Info is what the host delivers to HandleInfo: an [ExitInfo], a
// [DownInfo], a [TimerInfo] or an [EventInfo].
type Info interface{ isInfo() }

// ExitInfo: a linked instance exited (with trap-exits on), or an exit
// signal arrived.
type ExitInfo struct {
	From   Pid
	Reason ExitReason
}

// DownInfo: a monitored instance exited.
type DownInfo struct {
	Monitor MonitorRef
	Pid     Pid
	Reason  ExitReason
}

// TimerInfo: a timer fired, with the message it carried.
type TimerInfo struct {
	Timer TimerRef
	Msg   Envelope
}

// EventInfo is a named host event. Ignore names not known: later hosts add
// events. The SDK handles `log-level` itself.
type EventInfo struct {
	Name string
	Msg  Envelope
}

func (ExitInfo) isInfo()  {}
func (DownInfo) isInfo()  {}
func (TimerInfo) isInfo() {}
func (EventInfo) isInfo() {}

// infoFromHost copies info and frees what it holds.
func infoFromHost(info types.Info) Info {
	switch {
	case info.Exit() != nil:
		e := info.Exit()
		return ExitInfo{From: pidFromHost(e.F0), Reason: exitFromHost(e.F1)}
	case info.Down() != nil:
		d := info.Down()
		return DownInfo{Monitor: MonitorRef(d.F0), Pid: pidFromHost(d.F1), Reason: exitFromHost(d.F2)}
	case info.Timer() != nil:
		t := info.Timer()
		return TimerInfo{Timer: TimerRef(t.F0), Msg: takeEnvelope(t.F1)}
	default:
		ev := info.HostEvent()
		return EventInfo{Name: abi.String(ev.F0), Msg: takeEnvelope(ev.F1)}
	}
}
