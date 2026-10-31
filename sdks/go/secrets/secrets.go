// Package secrets reads secrets the operator binds, through
// bulkhead:secrets. Declare the `bulkhead:secrets` capability in the manifest
// with the secrets' names.
package secrets

import (
	"errors"
	"strings"

	"github.com/jrobhoward/bulkhead/sdks/go/internal/abi"
	"github.com/jrobhoward/bulkhead/sdks/go/internal/gen/bulkhead/secrets/store"
)

var (
	// ErrAccessDenied: the capability is not granted, or the manifest does
	// not list the secret.
	ErrAccessDenied = errors.New("access denied")
	// ErrNotFound: the operator bound no value to the secret.
	ErrNotFound = errors.New("not found")
)

// UnavailableError: the secret's source could not be read.
type UnavailableError struct{ Msg string }

func (e *UnavailableError) Error() string { return "unavailable: " + e.Msg }

// Get reads secret name.
func Get(name string) ([]byte, error) {
	r := store.Get(name)
	if e := r.Err(); e != nil {
		switch {
		case e.AccessDenied():
			return nil, ErrAccessDenied
		case e.NotFound():
			return nil, ErrNotFound
		default:
			return nil, &UnavailableError{Msg: abi.String(*e.Unavailable())}
		}
	}
	return abi.Bytes(*r.OK()), nil
}

// GetString reads secret name as text, with one trailing newline removed
// (files written by echo have one).
func GetString(name string) (string, error) {
	b, err := Get(name)
	if err != nil {
		return "", err
	}
	s := strings.TrimSuffix(string(b), "\n")
	if len(s) < len(b) {
		s = strings.TrimSuffix(s, "\r")
	}
	return s, nil
}
