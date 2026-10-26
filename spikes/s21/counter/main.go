// Spike S21: a Paddock app in Go (TinyGo), against the raw WIT bindings in
// gen/ (wit-bindgen-go). The counter example's text protocol, plus commands
// that misbehave so the host's limits can be checked on a TinyGo guest.
//
// Calls (text/plain): inc, get, after <ms> (a timer that incs through
// handle-info), self (a call to its own registered name, which must fail),
// panic, spin, alloc <mib>.
package main

import (
	"encoding/binary"
	"encoding/json"
	"strconv"
	"strings"

	"example.com/s21counter/gen/s21/go/app"
	"example.com/s21counter/gen/paddock/runtime/process"
	"example.com/s21counter/gen/paddock/runtime/types"
	"go.bytecodealliance.org/cm"
)

const version = "go-1"

var (
	count   uint64
	ballast [][]byte
)

func text(s string) types.Envelope {
	return types.Envelope{
		ContentType: "text/plain",
		Body:        cm.ToList([]byte(s)),
	}
}

func init() {
	app.Exports.Init = func(args cm.List[uint8]) cm.Result[string, struct{}, string] {
		var a struct {
			Register string `json:"register"`
		}
		if b := args.Slice(); len(b) > 0 {
			if err := json.Unmarshal(b, &a); err != nil {
				return cm.Err[cm.Result[string, struct{}, string]]("start_args: " + err.Error())
			}
		}
		if a.Register != "" {
			r := process.Register(a.Register)
			if e := r.Err(); e != nil {
				return cm.Err[cm.Result[string, struct{}, string]]("register " + a.Register + ": " + e.String())
			}
		}
		return cm.OK[cm.Result[string, struct{}, string]](struct{}{})
	}

	app.Exports.HandleCall = func(_ app.CallRef, msg app.Envelope) types.CallResult {
		cmd := string(msg.Body.Slice())
		release(msg)
		return types.CallResultReply(text(command(cmd)))
	}

	app.Exports.HandleCast = func(msg app.Envelope) {
		cmd := string(msg.Body.Slice())
		release(msg)
		command(cmd)
	}

	app.Exports.HandleInfo = func(msg types.Info) {
		if t := msg.Timer(); t != nil {
			cmd := string(t.F1.Body.Slice())
			release(t.F1)
			command(cmd)
		}
	}

	app.Exports.Terminate = func(types.ExitReason) {}

	app.Exports.SaveSnapshot = func() cm.List[uint8] {
		return cm.ToList(binary.LittleEndian.AppendUint64(nil, count))
	}

	app.Exports.LoadSnapshot = func(from string, snap cm.List[uint8]) cm.Result[string, struct{}, string] {
		b := snap.Slice()
		if len(b) != 8 {
			return cm.Err[cm.Result[string, struct{}, string]]("snapshot from " + from + ": want 8 bytes")
		}
		count = binary.LittleEndian.Uint64(b)
		return cm.OK[cm.Result[string, struct{}, string]](struct{}{})
	}
}

func command(cmd string) string {
	word, arg, _ := strings.Cut(strings.TrimSpace(cmd), " ")
	switch word {
	case "inc":
		count++
		return strconv.FormatUint(count, 10)
	case "get":
		return version + " count=" + strconv.FormatUint(count, 10)
	case "after":
		ms, _ := strconv.Atoi(arg)
		process.SendAfter(uint32(ms), text("inc"))
		return "ok"
	case "self":
		p := process.Whereis("gocounter")
		if p.None() {
			return "not registered"
		}
		r := process.Call(*p.Some(), text("get"), 1000)
		if e := r.Err(); e != nil {
			return "call failed: " + e.String()
		}
		return "call replied: " + string(r.OK().Body.Slice())
	case "panic":
		panic("asked to panic")
	case "spin":
		for {
		}
	case "alloc":
		mib, _ := strconv.Atoi(arg)
		for i := 0; i < mib; i++ {
			b := make([]byte, 1<<20)
			for j := range b {
				b[j] = 1
			}
			ballast = append(ballast, b)
		}
		return "held " + strconv.Itoa(len(ballast)) + " MiB"
	}
	return "commands: inc, get, after <ms>, self, panic, spin, alloc <mib>"
}

// main is required by TinyGo but never runs: the component exports no
// wasi:cli/run, so the host drives it through the app world only.
func main() {}
