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

// KeyedEntry is one entry a scan found.
type KeyedEntry struct {
	Key      string
	Value    []byte
	Revision uint64
}

// Page is one page of a [Scanner].
type Page struct {
	// Entries in key order, or downward with [Scanner.Reverse].
	Entries []KeyedEntry
	// Next is where the next page starts, for [Scanner.Page]; More is
	// false when the scan is complete.
	Next string
	More bool
	// Revision is the bucket's revision when the page was read: [Resume]
	// from it to see every change after.
	Revision uint64
}

// Scanner reads a bucket's entries in key order (the bytes of their UTF-8),
// like ETS's ordered_set: every key, or those with a prefix or between two
// keys, upward or downward. Read a page at a time with [Scanner.Page], or
// every entry with [Scanner.Each]. Each page is read at one revision; a
// scan of many pages is not one snapshot.
//
//	err := watch.Scan("orders").Prefix("c42/").Reverse().Each(func(e watch.KeyedEntry) bool {
//		...
//		return true // false stops
//	})
type Scanner struct {
	bucket string
	query  watch.ScanQuery
}

// Scan scans bucket; every key, upward, until narrowed.
func Scan(bucket string) *Scanner {
	return &Scanner{bucket: bucket}
}

// Prefix keeps only keys starting with prefix.
func (s *Scanner) Prefix(prefix string) *Scanner {
	s.query.Prefix = cm.Some(prefix)
	return s
}

// Start keeps only keys from key on (inclusive).
func (s *Scanner) Start(key string) *Scanner {
	s.query.Start = cm.Some(key)
	return s
}

// End keeps only keys before key (exclusive).
func (s *Scanner) End(key string) *Scanner {
	s.query.End = cm.Some(key)
	return s
}

// Reverse scans downward, from the last key in range.
func (s *Scanner) Reverse() *Scanner {
	s.query.Reverse = true
	return s
}

// Limit reads at most n entries per page (the host caps it at 1,000).
func (s *Scanner) Limit(n uint32) *Scanner {
	s.query.Limit = n
	return s
}

// Page reads one page: from the start when cursor is empty, else from a
// previous page's Next.
func (s *Scanner) Page(cursor string) (Page, error) {
	query := s.query
	if cursor != "" {
		query.Cursor = cm.Some(cursor)
	}
	r := watch.Scan(s.bucket, query)
	if e := r.Err(); e != nil {
		return Page{}, takeError(*e)
	}
	got := r.OK()
	page := Page{Revision: got.Revision}
	for _, e := range got.Entries.Slice() {
		page.Entries = append(page.Entries, KeyedEntry{
			Key:      abi.String(e.Key),
			Value:    abi.Bytes(e.Value),
			Revision: e.Revision,
		})
	}
	abi.FreeList(got.Entries)
	if next := got.Next.Some(); next != nil {
		page.Next, page.More = abi.String(*next), true
	}
	return page, nil
}

// Each calls f with every entry, reading a page at a time, until f returns
// false or the scan ends.
func (s *Scanner) Each(f func(KeyedEntry) bool) error {
	cursor := ""
	for {
		page, err := s.Page(cursor)
		if err != nil {
			return err
		}
		for _, e := range page.Entries {
			if !f(e) {
				return nil
			}
		}
		if !page.More {
			return nil
		}
		cursor = page.Next
	}
}

func one(s *Scanner) (KeyedEntry, bool, error) {
	page, err := s.Page("")
	if err != nil || len(page.Entries) == 0 {
		return KeyedEntry{}, false, err
	}
	return page.Entries[0], true, nil
}

// First is the entry with the smallest key (ETS first).
func First(bucket string) (KeyedEntry, bool, error) {
	return one(Scan(bucket).Limit(1))
}

// Last is the entry with the largest key (ETS last).
func Last(bucket string) (KeyedEntry, bool, error) {
	return one(Scan(bucket).Reverse().Limit(1))
}

// Next is the entry with the smallest key after key, which need not exist
// (ETS next).
func Next(bucket, key string) (KeyedEntry, bool, error) {
	page, err := Scan(bucket).Start(key).Limit(2).Page("")
	if err != nil {
		return KeyedEntry{}, false, err
	}
	for _, e := range page.Entries {
		if e.Key > key {
			return e, true, nil
		}
	}
	return KeyedEntry{}, false, nil
}

// Prev is the entry with the largest key before key, which need not exist
// (ETS prev).
func Prev(bucket, key string) (KeyedEntry, bool, error) {
	return one(Scan(bucket).End(key).Reverse().Limit(1))
}
