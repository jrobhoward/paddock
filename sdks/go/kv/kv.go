// Package kv is keyvalue buckets through wasi:keyvalue. Declare the
// `wasi:keyvalue` capability in the manifest with the app's buckets; shared
// buckets the operator grants are opened by name the same way.
//
//	b, err := kv.Open("data")
//	if err != nil { ... }
//	defer b.Close()
//	err = b.Set("greeting", []byte("hello"))
package kv

import (
	"errors"

	bulkhead "github.com/jrobhoward/bulkhead/sdks/go"
	"github.com/jrobhoward/bulkhead/sdks/go/internal/abi"
	"github.com/jrobhoward/bulkhead/sdks/go/internal/gen/wasi/keyvalue/atomics"
	"github.com/jrobhoward/bulkhead/sdks/go/internal/gen/wasi/keyvalue/store"
	"go.bytecodealliance.org/cm"
)

var (
	// ErrAccessDenied: the operator did not grant wasi:keyvalue, or the
	// manifest does not list the bucket.
	ErrAccessDenied = errors.New("access denied")
	// ErrNoSuchBucket: the host does not know the bucket.
	ErrNoSuchBucket = errors.New("no such bucket")
)

// StorageError is a storage failure, or an exceeded quota.
type StorageError struct{ Msg string }

func (e *StorageError) Error() string { return "storage: " + e.Msg }

// takeError copies e and frees its text.
func takeError(e store.Error) error {
	switch {
	case e.AccessDenied():
		return ErrAccessDenied
	case e.NoSuchStore():
		return ErrNoSuchBucket
	default:
		return &StorageError{Msg: abi.String(*e.Other())}
	}
}

// Bucket is an open bucket. Close it when done; an instance's buckets are
// closed when it exits.
type Bucket struct {
	b store.Bucket
}

// Open opens the bucket name.
func Open(name string) (*Bucket, error) {
	r := store.Open(name)
	if e := r.Err(); e != nil {
		return nil, takeError(*e)
	}
	return &Bucket{b: *r.OK()}, nil
}

// Close releases the bucket's handle.
func (b *Bucket) Close() { b.b.ResourceDrop() }

// Get reads key; found is false if it is absent.
func (b *Bucket) Get(key string) (value []byte, found bool, err error) {
	r := b.b.Get(key)
	if e := r.Err(); e != nil {
		return nil, false, takeError(*e)
	}
	if v := r.OK().Some(); v != nil {
		return abi.Bytes(*v), true, nil
	}
	return nil, false, nil
}

// Set writes key.
func (b *Bucket) Set(key string, value []byte) error {
	r := b.b.Set(key, cm.ToList(value))
	if e := r.Err(); e != nil {
		return takeError(*e)
	}
	return nil
}

// Delete removes key; removing an absent key is not an error.
func (b *Bucket) Delete(key string) error {
	r := b.b.Delete(key)
	if e := r.Err(); e != nil {
		return takeError(*e)
	}
	return nil
}

// Exists reports whether key is present.
func (b *Bucket) Exists(key string) (bool, error) {
	r := b.b.Exists(key)
	if e := r.Err(); e != nil {
		return false, takeError(*e)
	}
	return *r.OK(), nil
}

// Keys lists every key, following the host's pages.
func (b *Bucket) Keys() ([]string, error) {
	var keys []string
	cursor := cm.None[uint64]()
	for {
		r := b.b.ListKeys(cursor)
		if e := r.Err(); e != nil {
			return nil, takeError(*e)
		}
		page := r.OK()
		keys = append(keys, abi.Strings(page.Keys)...)
		next := page.Cursor.Some()
		if next == nil {
			return keys, nil
		}
		cursor = cm.Some(*next)
	}
}

// Increment adds delta to the counter at key (an absent key counts from
// 0) and returns the new count.
func (b *Bucket) Increment(key string, delta uint64) (uint64, error) {
	r := atomics.Increment(b.b, key, delta)
	if e := r.Err(); e != nil {
		return 0, takeError(*e)
	}
	return *r.OK(), nil
}

// GetValue reads key as the CBOR of v; found is false if it is absent.
func (b *Bucket) GetValue(key string, v any) (found bool, err error) {
	raw, found, err := b.Get(key)
	if err != nil || !found {
		return found, err
	}
	return true, bulkhead.UnmarshalCBOR(raw, v)
}

// SetValue writes v at key as CBOR.
func (b *Bucket) SetValue(key string, v any) error {
	raw, err := bulkhead.MarshalCBOR(v)
	if err != nil {
		return err
	}
	return b.Set(key, raw)
}
