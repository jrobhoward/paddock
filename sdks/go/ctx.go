package bulkhead

import (
	"time"

	"github.com/jrobhoward/bulkhead/sdks/go/internal/gen/bulkhead/runtime/process"
)

// Ctx is messaging, links, monitors, timers and the registry, passed to
// every callback. Each method is one host function; a component imports
// only the ones its code calls.
type Ctx struct{}

func millis(d time.Duration) uint32 {
	ms := d.Milliseconds()
	switch {
	case ms < 0:
		return 0
	case ms > int64(^uint32(0)):
		return ^uint32(0)
	default:
		return uint32(ms)
	}
}

// Pid is this instance's pid.
func (*Ctx) Pid() Pid {
	return pidFromHost(process.Self())
}

// Cast sends without waiting. At most once; a message to a dead pid is
// dropped silently.
func (*Ctx) Cast(to Pid, msg Envelope) {
	process.Cast(to.host(), msg.host())
}

// Call sends and waits for the reply. This instance handles no other
// message meanwhile; the time spent waiting does not count against its
// callback deadline. A call to this instance's own pid traps: it could
// never be answered. The error is a *[CallError].
func (*Ctx) Call(to Pid, msg Envelope, timeout time.Duration) (Envelope, error) {
	result := process.Call(to.host(), msg.host(), millis(timeout))
	if err := result.Err(); err != nil {
		return Envelope{}, callErrorFromHost(*err)
	}
	return takeEnvelope(*result.OK()), nil
}

// Reply answers a call that returned [Later]. A second answer to the same
// call is dropped.
func (*Ctx) Reply(call CallRef, msg Envelope) {
	process.Reply(process.CallRef(call), msg.host())
}

// Link links this instance to other: when either exits abnormally, the
// other exits too, or with trap-exits on receives an [ExitInfo].
func (*Ctx) Link(other Pid) { process.Link(other.host()) }

// Unlink removes a link.
func (*Ctx) Unlink(other Pid) { process.Unlink(other.host()) }

// TrapExits turns exits from links into [ExitInfo] messages, or back.
func (*Ctx) TrapExits(on bool) { process.TrapExits(on) }

// Monitor watches other: when it exits, this instance receives a
// [DownInfo] with the returned reference.
func (*Ctx) Monitor(other Pid) MonitorRef {
	return MonitorRef(process.Monitor(other.host()))
}

// Demonitor removes a monitor.
func (*Ctx) Demonitor(monitor MonitorRef) { process.Demonitor(uint64(monitor)) }

// Register registers a node-local name listed in the manifest. The error is
// a *[RegisterError].
func (*Ctx) Register(name string) error {
	result := process.Register(name)
	if err := result.Err(); err != nil {
		if holder := err.Taken(); holder != nil {
			return &RegisterError{Taken: true, Holder: pidFromHost(*holder)}
		}
		return &RegisterError{}
	}
	return nil
}

// Whereis looks up a registered name, or an app's name (one of its
// instances, round-robin).
func (*Ctx) Whereis(name string) (Pid, bool) {
	found := process.Whereis(name)
	if p := found.Some(); p != nil {
		return pidFromHost(*p), true
	}
	return Pid{}, false
}

// SendAfter delivers msg to this instance as a [TimerInfo] after delay.
func (*Ctx) SendAfter(delay time.Duration, msg Envelope) TimerRef {
	return TimerRef(process.SendAfter(millis(delay), msg.host()))
}

// CancelTimer cancels a timer that has not fired.
func (*Ctx) CancelTimer(timer TimerRef) { process.CancelTimer(uint64(timer)) }

// Exit sends an exit signal. Killed cannot be trapped; Normal to another
// non-trapping instance is ignored.
func (*Ctx) Exit(other Pid, reason ExitReason) {
	process.Exit(other.host(), reason.host())
}
