//go:build !s21free

package main

import "example.com/s21counter/gen/paddock/runtime/types"

// release does nothing: the default build leaks each envelope (release.go).
func release(types.Envelope) {}
