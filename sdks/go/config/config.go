// Package config is the app's configuration through wasi:config: the
// manifest's values, overridden per node by the operator. Declare the
// `wasi:config` capability in the manifest.
package config

import (
	"errors"

	"github.com/jrobhoward/bulkhead/sdks/go/internal/abi"
	"github.com/jrobhoward/bulkhead/sdks/go/internal/gen/wasi/config/store"
)

// ErrAccessDenied: the operator did not grant wasi:config.
var ErrAccessDenied = errors.New("access denied")

// Error is a failure reading the configuration.
type Error struct{ Msg string }

func (e *Error) Error() string { return "config: " + e.Msg }

func takeError(e store.Error) error {
	if msg := e.IO(); msg != nil {
		text := abi.String(*msg)
		if text == "access-denied" {
			return ErrAccessDenied
		}
		return &Error{Msg: text}
	}
	return &Error{Msg: abi.String(*e.Upstream())}
}

// Get reads key; found is false when it is not set.
func Get(key string) (value string, found bool, err error) {
	r := store.Get(key)
	if e := r.Err(); e != nil {
		return "", false, takeError(*e)
	}
	if v := r.OK().Some(); v != nil {
		return abi.String(*v), true, nil
	}
	return "", false, nil
}

// All reads every key and value.
func All() (map[string]string, error) {
	r := store.GetAll()
	if e := r.Err(); e != nil {
		return nil, takeError(*e)
	}
	list := *r.OK()
	out := make(map[string]string, list.Len())
	for _, kv := range list.Slice() {
		out[abi.String(kv[0])] = abi.String(kv[1])
	}
	abi.FreeList(list)
	return out, nil
}
