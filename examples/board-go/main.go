// The board example in Go, on the Go SDK (sdks/go): the same app as
// examples/board, so the same manifests, node tests and tutorial run with
// either. A counter kept on a bulletin board, the node's shared `board`
// bucket on disk; the instance holds no state of its own, so the count
// survives a crash and its restart. One component, three ways to run it:
//
//   - board (bulkhead.toml) serves HTTP through the node's ingress. Every
//     response carries x-bulkhead-pid, the instance that answered.
//     GET /board: {"count": n, "pid": "<...>"}. POST /board: add one (or the
//     body's number) and return the same. DELETE /board: reset to 0.
//     POST /board/crash: trap, as a bug would; the supervisor restarts the
//     instance with a new incarnation.
//   - board-watch (watch.toml, start args {"watch": true}) watches the
//     counter and logs every change; a call replies how many it has seen.
//   - board as a hot standby (hot-standby.toml, start args
//     {"replicate": true}): every instance joins the `board` process group;
//     the primary writes each count to the other members' boards before it
//     answers, and the standby refuses writes (503).
//
// Build with sdks/go/build.sh examples/board-go <out.wasm>.
package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"strconv"
	"strings"
	"time"

	bulkhead "github.com/jrobhoward/bulkhead/sdks/go"
	"github.com/jrobhoward/bulkhead/sdks/go/groups"
	"github.com/jrobhoward/bulkhead/sdks/go/http"
	"github.com/jrobhoward/bulkhead/sdks/go/kv"
	"github.com/jrobhoward/bulkhead/sdks/go/log"
	"github.com/jrobhoward/bulkhead/sdks/go/watch"
)

// The shared bucket, the key the count is kept under, and the process group
// the instances join when they replicate.
const (
	board          = "board"
	key            = "count"
	group          = "board"
	replicaTimeout = time.Second
)

type args struct {
	Watch     bool `json:"watch"`
	Replicate bool `json:"replicate"`
}

// role is this node's part in its HA pair, from the failover-role host
// event.
type role int

const (
	alone role = iota
	primary
	standby
)

var roleNames = [...]string{"Alone", "Primary", "Standby"}

// web serves the board over HTTP.
type web struct {
	replicate bool
	role      role
}

// watcher logs the board's changes.
type watcher struct {
	seen uint64
}

func read(b *kv.Bucket) (int64, error) {
	var count int64
	_, err := b.GetValue(key, &count)
	return count, err
}

// replicateTo writes count to every other member of the group, and returns
// how many took it.
func replicateTo(ctx *bulkhead.Ctx, count int64) int {
	me := ctx.Pid()
	members, _ := groups.Members(group)
	took := 0
	for _, other := range members {
		if other == me {
			continue
		}
		msg := bulkhead.Text(fmt.Sprintf("replicate:%d", count))
		if _, err := ctx.Call(other, msg, replicaTimeout); err != nil {
			log.Warnf("replica %s missed count %d: %v", other, count, err)
			continue
		}
		took++
	}
	return took
}

type reply struct {
	Count    int64  `json:"count"`
	Pid      string `json:"pid"`
	Replicas *int   `json:"replicas,omitempty"`
}

func (w *web) serve(ctx *bulkhead.Ctx, req http.Request) (http.Response, error) {
	pid := ctx.Pid().String()
	b, err := kv.Open(board)
	if err != nil {
		return http.Response{}, err
	}
	defer b.Close()
	write := req.Method == "POST" || req.Method == "DELETE"
	if write && w.replicate && w.role == standby {
		return http.Text(503, "standby: writes go to the primary"), nil
	}
	var count int64
	switch {
	case req.Method == "GET" && req.Path == "/board":
		if count, err = read(b); err != nil {
			return http.Response{}, err
		}
	case req.Method == "POST" && req.Path == "/board":
		by := int64(1)
		if text, ok := req.Text(); ok {
			if n, err := strconv.ParseInt(strings.TrimSpace(text), 10, 64); err == nil {
				by = n
			}
		}
		if count, err = read(b); err != nil {
			return http.Response{}, err
		}
		count += by
		if err = b.SetValue(key, count); err != nil {
			return http.Response{}, err
		}
	case req.Method == "DELETE" && req.Path == "/board":
		if err = b.SetValue(key, int64(0)); err != nil {
			return http.Response{}, err
		}
	case req.Method == "POST" && req.Path == "/board/crash":
		log.Warnf("crashing on request")
		// A trap, as a bug would raise; the supervisor restarts us.
		panic("crash requested")
	case req.Path == "/board" || req.Path == "/board/crash":
		return http.Text(405, "GET, POST or DELETE /board; POST /board/crash"), nil
	default:
		return http.Text(404, "not here"), nil
	}
	body := reply{Count: count, Pid: pid}
	if write && w.replicate {
		took := replicateTo(ctx, count)
		body.Replicas = &took
	}
	resp, err := http.JSON(200, body)
	if err != nil {
		return http.Response{}, err
	}
	return resp.Header("x-bulkhead-pid", pid), nil
}

func (w *web) HandleCall(ctx *bulkhead.Ctx, _ bulkhead.CallRef, msg bulkhead.Envelope) bulkhead.Reply {
	if req, ok := http.RequestFrom(msg); ok {
		resp, err := w.serve(ctx, req)
		if err != nil {
			resp = http.Text(500, err.Error())
		}
		return bulkhead.Now(resp.Envelope())
	}
	text, _ := msg.AsText()
	n, ok := strings.CutPrefix(text, "replicate:")
	if !ok {
		return bulkhead.Now(bulkhead.Text("send HTTP requests to /board"))
	}
	// The primary's write, for this node's board.
	count, err := strconv.ParseInt(n, 10, 64)
	if err != nil {
		return bulkhead.Now(bulkhead.Text("error: " + err.Error()))
	}
	b, err := kv.Open(board)
	if err == nil {
		err = b.SetValue(key, count)
		b.Close()
	}
	if err != nil {
		return bulkhead.Now(bulkhead.Text("error: " + err.Error()))
	}
	return bulkhead.Now(bulkhead.Text("ok"))
}

func (w *web) HandleInfo(_ *bulkhead.Ctx, info bulkhead.Info) {
	ev, ok := info.(bulkhead.EventInfo)
	if !ok || ev.Name != "failover-role" {
		return
	}
	switch text, _ := ev.Msg.AsText(); text {
	case "primary":
		w.role = primary
	case "standby":
		w.role = standby
	default:
		w.role = alone
	}
	log.Infof("role: %s", roleNames[w.role])
}

func (w *watcher) HandleCall(*bulkhead.Ctx, bulkhead.CallRef, bulkhead.Envelope) bulkhead.Reply {
	return bulkhead.Now(bulkhead.Text(strconv.FormatUint(w.seen, 10)))
}

func (w *watcher) HandleInfo(_ *bulkhead.Ctx, info bulkhead.Info) {
	change, ok := watch.ChangeFromInfo(info)
	if !ok {
		return
	}
	w.seen++
	b, err := kv.Open(change.Bucket)
	if err != nil {
		return
	}
	defer b.Close()
	var count int64
	found, err := b.GetValue(change.Key, &count)
	switch {
	case err != nil:
	case found:
		log.Infof("board: count is %d (revision %d)", count, change.Revision)
	default:
		log.Infof("board: count removed (revision %d)", change.Revision)
	}
}

func start(_ *bulkhead.Ctx, raw []byte) (bulkhead.App, error) {
	var a args
	if len(raw) > 0 {
		if err := json.Unmarshal(raw, &a); err != nil {
			return nil, errors.New("start_args: " + err.Error())
		}
	}
	if !a.Watch {
		if a.Replicate {
			if err := groups.Join(group); err != nil {
				return nil, err
			}
		}
		return &web{replicate: a.Replicate}, nil
	}
	started, err := watch.Watch(board, key)
	if err != nil {
		return nil, err
	}
	log.Infof("watching %s/%s from revision %d", board, key, started.Revision)
	return &watcher{}, nil
}

func init() {
	bulkhead.Export(bulkhead.Def{Init: start})
}

func main() {}
