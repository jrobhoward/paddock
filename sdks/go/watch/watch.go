// Package watch is revisions, compare-and-swap and change watches on
// keyvalue buckets, through paddock:watch, under the wasi:keyvalue
// capability: a bucket is reachable here exactly when kv.Open would open
// it.
//
// Every change to a bucket raises its revision by one and stamps the keys
// it wrote with that revision. A watch's changes arrive in HandleInfo as
// `kv-changed` host events; [ChangeFromInfo] reads them.
package watch

import (
	"encoding/json"
	"errors"
	"fmt"

	paddock "github.com/jrobhoward/paddock/sdks/go"
	"github.com/jrobhoward/paddock/sdks/go/internal/abi"
	"github.com/jrobhoward/paddock/sdks/go/internal/gen/paddock/watch/watch"
	"go.bytecodealliance.org/cm"
)

// Event is the name of the host event that carries a change.
const Event = "kv-changed"

var (
	// ErrAccessDenied: the capability is not granted, the manifest does not
	// list the bucket, or a write went to a read-only shared bucket.
	ErrAccessDenied = errors.New("access denied")
	// ErrNoSuchBucket: the node does not configure this shared bucket.
	ErrNoSuchBucket = errors.New("no such bucket")
	// ErrLimitReached: over limits.watches_max.
	ErrLimitReached = errors.New("watch limit reached")
	// ErrUnavailable: a replicated bucket with no primary that can take
	// the operation now, or a write whose outcome is unknown.
	ErrUnavailable = errors.New("unavailable")
)

// TooOldError: the bucket cannot replay changes after the revision asked.
// Reread the watched keys, then watch from Revision (the current one).
type TooOldError struct{ Revision uint64 }

func (e *TooOldError) Error() string {
	return fmt.Sprintf("revision too old (bucket at %d)", e.Revision)
}

// ConflictError: compare-and-swap found the key at another revision, or
// absent when Found is nil.
type ConflictError struct{ Found *uint64 }

func (e *ConflictError) Error() string {
	if e.Found == nil {
		return "conflict (key absent)"
	}
	return fmt.Sprintf("conflict (key at %d)", *e.Found)
}

// StorageError is a storage failure, or an exceeded quota.
type StorageError struct{ Msg string }

func (e *StorageError) Error() string { return "storage: " + e.Msg }

func takeError(e watch.Error) error {
	switch {
	case e.AccessDenied():
		return ErrAccessDenied
	case e.NoSuchBucket():
		return ErrNoSuchBucket
	case e.LimitReached():
		return ErrLimitReached
	case e.Unavailable():
		return ErrUnavailable
	case e.TooOld() != nil:
		return &TooOldError{Revision: *e.TooOld()}
	case e.Conflict() != nil:
		if found := e.Conflict().Some(); found != nil {
			r := *found
			return &ConflictError{Found: &r}
		}
		return &ConflictError{}
	default:
		return &StorageError{Msg: abi.String(*e.Other())}
	}
}

// Started is a started watch: its id, and the bucket's revision when it
// started (read the watched keys, and resume from here after a restart).
type Started struct {
	ID       uint64
	Revision uint64
}

// Entry is a value and the revision it was written at.
type Entry struct {
	Value    []byte
	Revision uint64
}

// Change is a change notice: the key changed (it may have been deleted),
// and its latest revision when the notice was taken.
type Change struct {
	Watch    uint64 `json:"watch"`
	Bucket   string `json:"bucket"`
	Key      string `json:"key"`
	Revision uint64 `json:"revision"`
}

// ChangeFromInfo is the change an info carries, if it is a change notice.
func ChangeFromInfo(info paddock.Info) (Change, bool) {
	ev, ok := info.(paddock.EventInfo)
	if !ok || ev.Name != Event {
		return Change{}, false
	}
	var c Change
	if err := json.Unmarshal(ev.Msg.Body, &c); err != nil {
		return Change{}, false
	}
	return c, true
}

func start(bucket, key string, prefix bool, after cm.Option[uint64]) (Started, error) {
	r := watch.Watch(bucket, key, prefix, after)
	if e := r.Err(); e != nil {
		return Started{}, takeError(*e)
	}
	s := r.OK()
	return Started{ID: s.ID, Revision: s.Revision}, nil
}

// Watch watches key in bucket.
func Watch(bucket, key string) (Started, error) {
	return start(bucket, key, false, cm.None[uint64]())
}

// WatchPrefix watches every key in bucket starting with prefix.
func WatchPrefix(bucket, prefix string) (Started, error) {
	return start(bucket, prefix, true, cm.None[uint64]())
}

// Resume watches as Watch or WatchPrefix, first sending a notice for every
// matching key changed after revision after; a *[TooOldError] when the
// bucket cannot replay them.
func Resume(bucket, key string, prefix bool, after uint64) (Started, error) {
	return start(bucket, key, prefix, cm.Some(after))
}

// Unwatch ends a watch.
func Unwatch(id uint64) error {
	r := watch.Unwatch(id)
	if e := r.Err(); e != nil {
		return takeError(*e)
	}
	return nil
}

func entry(r cm.Result[watch.OptionEntryShape, cm.Option[watch.Entry], watch.Error]) (Entry, bool, error) {
	if e := r.Err(); e != nil {
		return Entry{}, false, takeError(*e)
	}
	if found := r.OK().Some(); found != nil {
		return Entry{Value: abi.Bytes(found.Value), Revision: found.Revision}, true, nil
	}
	return Entry{}, false, nil
}

// Get reads key with its revision.
func Get(bucket, key string) (Entry, bool, error) {
	return entry(watch.Get(bucket, key))
}

// GetLinearizable reads key as Get does, from the primary of a replicated
// bucket.
func GetLinearizable(bucket, key string) (Entry, bool, error) {
	return entry(watch.GetLinearizable(bucket, key))
}

// Revision is bucket's current revision.
func Revision(bucket string) (uint64, error) {
	r := watch.Revision(bucket)
	if e := r.Err(); e != nil {
		return 0, takeError(*e)
	}
	return *r.OK(), nil
}

// CompareAndSwap writes value at key if the key is at revision expected
// (absent, when expected is nil), and returns the new revision; a
// *[ConflictError] otherwise.
func CompareAndSwap(bucket, key string, expected *uint64, value []byte) (uint64, error) {
	want := cm.None[uint64]()
	if expected != nil {
		want = cm.Some(*expected)
	}
	r := watch.CompareAndSwap(bucket, key, want, cm.ToList(value))
	if e := r.Err(); e != nil {
		return 0, takeError(*e)
	}
	return *r.OK(), nil
}

// CompareAndDelete deletes key if it is at revision expected, and returns
// the bucket's new revision.
func CompareAndDelete(bucket, key string, expected uint64) (uint64, error) {
	r := watch.CompareAndDelete(bucket, key, expected)
	if e := r.Err(); e != nil {
		return 0, takeError(*e)
	}
	return *r.OK(), nil
}
