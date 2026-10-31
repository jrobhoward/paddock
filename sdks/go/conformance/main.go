// The Go SDK's conformance app: the command protocol of
// crates/paddock-fixture-conformance, which paddock-host's
// tests/conformance.rs drives. Build with ../build.sh and run the suite
// with PADDOCK_CONFORMANCE_APP set to the component.
package main

import (
	"errors"
	"fmt"
	"slices"
	"strconv"
	"strings"
	"time"

	paddock "github.com/jrobhoward/paddock/sdks/go"
	"github.com/jrobhoward/paddock/sdks/go/config"
	"github.com/jrobhoward/paddock/sdks/go/groups"
	"github.com/jrobhoward/paddock/sdks/go/kv"
	"github.com/jrobhoward/paddock/sdks/go/log"
	"github.com/jrobhoward/paddock/sdks/go/metrics"
	"github.com/jrobhoward/paddock/sdks/go/secrets"
	"github.com/jrobhoward/paddock/sdks/go/watch"
)

// Kept is what a snapshot carries, in the protocol's order.
type Kept struct {
	Count    uint64   `cbor:"count"`
	Notes    []string `cbor:"notes"`
	Deferred []uint64 `cbor:"deferred"`
	Refuse   bool     `cbor:"refuse"`
}

const formatVersion = 1

type conformance struct {
	kept         Kept
	args         string
	fired        []string
	infos        []string
	changes      []string
	health       string
	healthChecks uint64
	loadedFrom   string
	hoard        [][]byte
	onTerminate  *paddock.Pid
}

// sink keeps the spin command's loop from being optimized away.
var sink uint64

func showPid(p paddock.Pid) string {
	return fmt.Sprintf("%d.%d.%d@%d", p.Node, p.ID&(1<<40-1), p.Incarnation, p.ID>>40)
}

func number(s string) uint64 {
	n, _ := strconv.ParseUint(s, 10, 64)
	return n
}

func parsePid(s string) paddock.Pid {
	fields, creation, _ := strings.Cut(s, "@")
	parts := strings.Split(fields, ".")
	get := func(i int) uint64 {
		if i < len(parts) {
			return number(parts[i])
		}
		return 0
	}
	return paddock.Pid{
		Node:        uint32(get(0)),
		ID:          number(creation)<<40 | get(1),
		Incarnation: uint32(get(2)),
	}
}

func showExit(r paddock.ExitReason) string {
	if r.Kind == paddock.Crashed {
		return "crashed(" + r.Detail + ")"
	}
	return r.String()
}

func parseExit(s string) paddock.ExitReason {
	switch s {
	case "normal":
		return paddock.ExitReason{Kind: paddock.Normal}
	case "shutdown":
		return paddock.ExitReason{Kind: paddock.Shutdown}
	case "killed":
		return paddock.ExitReason{Kind: paddock.Killed}
	default:
		return paddock.CrashedWith(s)
	}
}

var callErrorNames = map[paddock.CallErrorKind]string{
	paddock.CallNoproc:       "noproc",
	paddock.CallNoconnection: "noconnection",
	paddock.CallTimeout:      "timeout",
	paddock.CallOverloaded:   "overloaded",
	paddock.CallTooLarge:     "too-large",
	paddock.CallAccessDenied: "access-denied",
}

func showCallError(err error) string {
	var ce *paddock.CallError
	if !errors.As(err, &ce) {
		return err.Error()
	}
	if ce.Kind == paddock.CallExited {
		return "exited:" + showExit(ce.Reason)
	}
	return callErrorNames[ce.Kind]
}

func register(ctx *paddock.Ctx, name string) string {
	err := ctx.Register(name)
	var re *paddock.RegisterError
	switch {
	case err == nil:
		return "ok"
	case errors.As(err, &re) && re.Taken:
		return "err:taken:" + showPid(re.Holder)
	default:
		return "err:access-denied"
	}
}

// serviceError is `err:access-denied` for a refusal for want of a grant or
// a manifest entry, and `err:<text>` otherwise.
func serviceError(err error) string {
	for _, denied := range []error{kv.ErrAccessDenied, watch.ErrAccessDenied, groups.ErrAccessDenied,
		config.ErrAccessDenied, secrets.ErrAccessDenied, metrics.ErrAccessDenied} {
		if errors.Is(err, denied) {
			return "err:access-denied"
		}
	}
	if errors.Is(err, secrets.ErrNotFound) {
		return "err:not-found"
	}
	return "err:" + err.Error()
}

func kvCommand(verb, arg string) (string, error) {
	parts := strings.SplitN(arg, ":", 3)
	for len(parts) < 3 {
		parts = append(parts, "")
	}
	b, err := kv.Open(parts[0])
	if err != nil {
		return "", err
	}
	defer b.Close()
	key := parts[1]
	switch verb {
	case "kv-set":
		return "ok", b.Set(key, []byte(parts[2]))
	case "kv-get":
		v, found, err := b.Get(key)
		if !found || err != nil {
			return "none", err
		}
		return string(v), nil
	case "kv-delete":
		return "ok", b.Delete(key)
	default:
		keys, err := b.Keys()
		slices.Sort(keys)
		return strings.Join(keys, ","), err
	}
}

func groupCommand(verb, arg string) (string, error) {
	group, rest, _ := strings.Cut(arg, ":")
	switch verb {
	case "join":
		return "ok", groups.Join(group)
	case "leave":
		return "ok", groups.Leave(group)
	case "members":
		members, err := groups.Members(group)
		shown := make([]string, 0, len(members))
		for _, m := range members {
			shown = append(shown, showPid(m))
		}
		slices.Sort(shown)
		return strings.Join(shown, ","), err
	default:
		n, err := groups.Publish(group, paddock.Text(rest))
		return strconv.FormatUint(uint64(n), 10), err
	}
}

func metricCommand(kind, arg string) (string, error) {
	name, n, _ := strings.Cut(arg, ":")
	value, _ := strconv.ParseFloat(n, 64)
	const help = "A conformance metric."
	switch kind {
	case "counter":
		c, err := metrics.NewCounter(name, help)
		if err == nil {
			c.Add(value)
		}
		return "ok", err
	case "gauge":
		g, err := metrics.NewGauge(name, help)
		if err == nil {
			g.Set(value)
		}
		return "ok", err
	default:
		h, err := metrics.NewHistogram(name, help, []float64{1, 10})
		if err == nil {
			h.Observe(value)
		}
		return "ok", err
	}
}

func scanCommand(arg string) (string, error) {
	f := strings.SplitN(arg, ":", 6)
	for len(f) < 6 {
		f = append(f, "")
	}
	s := watch.Scan(f[0])
	if f[1] != "" {
		s.Prefix(f[1])
	}
	if f[2] != "" {
		s.Start(f[2])
	}
	if f[3] != "" {
		s.End(f[3])
	}
	if f[4] == "down" {
		s.Reverse()
	}
	s.Limit(uint32(number(f[5])))
	var found []string
	err := s.Each(func(e watch.KeyedEntry) bool {
		found = append(found, e.Key+"="+string(e.Value))
		return true
	})
	return strings.Join(found, ","), err
}

func neighbour(verb, arg string) (string, error) {
	bucket, key, _ := strings.Cut(arg, ":")
	var e watch.KeyedEntry
	var found bool
	var err error
	switch verb {
	case "first":
		e, found, err = watch.First(bucket)
	case "last":
		e, found, err = watch.Last(bucket)
	case "next":
		e, found, err = watch.Next(bucket, key)
	default:
		e, found, err = watch.Prev(bucket, key)
	}
	if !found {
		return "none", err
	}
	return e.Key, err
}

func orServiceError(reply string, err error) string {
	if err != nil {
		return serviceError(err)
	}
	return reply
}

// run handles one command; reply is false for no answer.
func (c *conformance) run(ctx *paddock.Ctx, caller *paddock.CallRef, msg paddock.Envelope) (paddock.Envelope, bool) {
	command := string(msg.Body)
	verb, arg, _ := strings.Cut(command, ":")
	reply := func(s string) (paddock.Envelope, bool) { return paddock.Text(s), true }
	switch verb {
	case "echo":
		return reply(arg)
	case "mirror":
		return msg, true
	case "args":
		return reply(c.args)
	case "count":
		c.kept.Count++
		return reply(strconv.FormatUint(c.kept.Count, 10))
	case "note":
		c.kept.Notes = append(c.kept.Notes, arg)
		return reply("ok")
	case "notes":
		return reply(strings.Join(c.kept.Notes, ","))
	case "noreply":
		return paddock.Envelope{}, false
	case "defer":
		if caller != nil {
			c.kept.Deferred = append(c.kept.Deferred, uint64(*caller))
		}
		return paddock.Envelope{}, false
	case "answer":
		deferred := c.kept.Deferred
		c.kept.Deferred = nil
		for _, d := range deferred {
			ctx.Reply(paddock.CallRef(d), paddock.Text(arg))
		}
		return reply(fmt.Sprintf("answered:%d", len(deferred)))
	case "answer-now":
		if caller != nil {
			ctx.Reply(*caller, paddock.Text(arg))
		}
		return paddock.Envelope{}, false
	case "self":
		return reply(showPid(ctx.Pid()))
	case "call":
		parts := strings.SplitN(arg, ":", 3)
		for len(parts) < 3 {
			parts = append(parts, "")
		}
		got, err := ctx.Call(parsePid(parts[0]), paddock.Text(parts[2]), time.Duration(number(parts[1]))*time.Millisecond)
		if err != nil {
			return reply("err:" + showCallError(err))
		}
		return reply("ok:" + string(got.Body))
	case "cast":
		pid, inner, _ := strings.Cut(arg, ":")
		ctx.Cast(parsePid(pid), paddock.Text(inner))
		return reply("sent")
	case "register":
		return reply(register(ctx, arg))
	case "whereis":
		if p, ok := ctx.Whereis(arg); ok {
			return reply(showPid(p))
		}
		return reply("none")
	case "timer":
		ms, text, _ := strings.Cut(arg, ":")
		t := ctx.SendAfter(time.Duration(number(ms))*time.Millisecond, paddock.Text(text))
		return reply(strconv.FormatUint(uint64(t), 10))
	case "cancel":
		ctx.CancelTimer(paddock.TimerRef(number(arg)))
		return reply("cancelled")
	case "timers":
		return reply(strings.Join(c.fired, ","))
	case "link":
		ctx.Link(parsePid(arg))
		return reply("ok")
	case "unlink":
		ctx.Unlink(parsePid(arg))
		return reply("ok")
	case "trap-exits":
		ctx.TrapExits(arg == "on")
		return reply("ok")
	case "monitor":
		return reply(strconv.FormatUint(uint64(ctx.Monitor(parsePid(arg))), 10))
	case "demonitor":
		ctx.Demonitor(paddock.MonitorRef(number(arg)))
		return reply("ok")
	case "exit":
		pid, reason, found := strings.Cut(arg, ":")
		if !found {
			reason = "normal"
		}
		ctx.Exit(parsePid(pid), parseExit(reason))
		return reply("ok")
	case "infos":
		return reply(strings.Join(c.infos, ";"))
	case "on-terminate":
		p := parsePid(arg)
		c.onTerminate = &p
		return reply("ok")
	case "trap":
		panic("asked to trap")
	case "spin":
		for {
			sink = sink*6364136223846793005 + 1442695040888963407
		}
	case "alloc":
		block := make([]byte, number(arg)<<20)
		for i := range block {
			block[i] = 1
		}
		c.hoard = append(c.hoard, block)
		return reply("ok")
	case "refuse-upgrade":
		c.kept.Refuse = true
		return reply("ok")
	case "loaded-from":
		if c.loadedFrom == "" {
			return reply("none")
		}
		return reply(c.loadedFrom)
	case "health":
		c.health = ""
		if arg != "ok" {
			c.health = arg
		}
		return reply("ok")
	case "health-checks":
		return reply(strconv.FormatUint(c.healthChecks, 10))
	case "log":
		name, text, _ := strings.Cut(arg, ":")
		level, ok := log.ParseLevel(name)
		if !ok {
			level = log.Info
		}
		log.Write(level, text)
		return reply("ok")
	case "log-level":
		for l := log.Trace; l <= log.Critical; l++ {
			if log.Enabled(l) {
				return reply(l.String())
			}
		}
		return reply("critical")
	case "kv-set", "kv-get", "kv-delete", "kv-keys":
		return reply(orServiceError(kvCommand(verb, arg)))
	case "watch":
		bucket, key, _ := strings.Cut(arg, ":")
		started, err := watch.Watch(bucket, key)
		return reply(orServiceError(strconv.FormatUint(started.ID, 10), err))
	case "changes":
		return reply(strings.Join(c.changes, ","))
	case "scan":
		return reply(orServiceError(scanCommand(arg)))
	case "first", "last", "next", "prev":
		return reply(orServiceError(neighbour(verb, arg)))
	case "join", "leave", "members", "publish":
		return reply(orServiceError(groupCommand(verb, arg)))
	case "config":
		value, found, err := config.Get(arg)
		if !found && err == nil {
			value = "none"
		}
		return reply(orServiceError(value, err))
	case "secret":
		return reply(orServiceError(secrets.GetString(arg)))
	case "counter", "gauge", "histogram":
		return reply(orServiceError(metricCommand(verb, arg)))
	default:
		return reply("unknown command '" + command + "'")
	}
}

func (c *conformance) HandleCall(ctx *paddock.Ctx, call paddock.CallRef, msg paddock.Envelope) paddock.Reply {
	if out, ok := c.run(ctx, &call, msg); ok {
		return paddock.Now(out)
	}
	return paddock.Later()
}

func (c *conformance) HandleCast(ctx *paddock.Ctx, msg paddock.Envelope) {
	c.run(ctx, nil, msg)
}

func (c *conformance) HandleInfo(_ *paddock.Ctx, info paddock.Info) {
	if change, ok := watch.ChangeFromInfo(info); ok {
		c.changes = append(c.changes, fmt.Sprintf("%s@%d", change.Key, change.Revision))
		return
	}
	switch i := info.(type) {
	case paddock.TimerInfo:
		c.fired = append(c.fired, string(i.Msg.Body))
	case paddock.ExitInfo:
		c.infos = append(c.infos, fmt.Sprintf("exit:%s:%s", showPid(i.From), showExit(i.Reason)))
	case paddock.DownInfo:
		c.infos = append(c.infos, fmt.Sprintf("down:%d:%s:%s", i.Monitor, showPid(i.Pid), showExit(i.Reason)))
	case paddock.EventInfo:
		c.infos = append(c.infos, fmt.Sprintf("event:%s:%s", i.Name, i.Msg.Body))
	}
}

func (c *conformance) Terminate(ctx *paddock.Ctx, reason paddock.ExitReason) {
	if c.onTerminate != nil {
		ctx.Cast(*c.onTerminate, paddock.Text("note:terminate:"+showExit(reason)))
	}
}

func (c *conformance) Health(*paddock.Ctx) error {
	c.healthChecks++
	if c.health != "" {
		return errors.New(c.health)
	}
	return nil
}

func (c *conformance) SaveSnapshot() ([]byte, error) {
	return paddock.EncodeSnapshot(formatVersion, c.kept)
}

func start(ctx *paddock.Ctx, args []byte) (paddock.App, error) {
	raw := string(args)
	for _, word := range strings.Fields(strings.Trim(strings.TrimSpace(raw), "\"")) {
		if name, ok := strings.CutPrefix(word, "register:"); ok {
			if result := register(ctx, name); result != "ok" {
				return nil, errors.New(result)
			}
			continue
		}
		switch word {
		case "fail":
			return nil, errors.New("asked to fail")
		case "trap":
			panic("asked to trap in init")
		case "trap-exits":
			ctx.TrapExits(true)
		}
	}
	return &conformance{args: raw}, nil
}

func load(_ *paddock.Ctx, fromVersion string, snapshot []byte) (paddock.App, error) {
	var kept Kept
	if err := paddock.LoadSnapshot(snapshot, formatVersion, &kept); err != nil {
		return nil, err
	}
	if kept.Refuse {
		return nil, errors.New("asked to refuse")
	}
	return &conformance{kept: kept, loadedFrom: fromVersion}, nil
}

func init() {
	paddock.Export(paddock.Def{Init: start, LoadSnapshot: load})
}

func main() {}
