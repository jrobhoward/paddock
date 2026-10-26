//go:build s21free

package main

import (
	"unsafe"

	"example.com/s21counter/gen/paddock/runtime/types"
)

// TinyGo's cabi_realloc allocates with allocManual: the block stays live
// until freed. The host lowers every export argument through it, and the
// wit-bindgen-go export glue never frees them, so the app must, once it has
// copied what it keeps.
//
//go:linkname freeManual runtime.freeManual
func freeManual(ptr unsafe.Pointer)

func release(e types.Envelope) {
	if len(e.ContentType) > 0 {
		freeManual(unsafe.Pointer(unsafe.StringData(e.ContentType)))
	}
	if s := e.Schema.Some(); s != nil && len(*s) > 0 {
		freeManual(unsafe.Pointer(unsafe.StringData(*s)))
	}
	if e.Body.Len() > 0 {
		freeManual(unsafe.Pointer(e.Body.Data()))
	}
}
