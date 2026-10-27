package main

import (
	"unsafe"

	"example.com/pong/gen/paddock/runtime/types"
	"go.bytecodealliance.org/cm"
)

// TinyGo's cabi_realloc allocates with allocManual: a block stays live until
// it is freed. The host lowers every string and list it passes to an export
// through it, and the export glue wit-bindgen-go generates never frees them,
// so without these every message leaks until the instance reaches
// memory_max (spike S21). Free each argument once what is kept has been
// copied out of it. The results of imports (a `call`'s reply) are allocated
// the same way.
//
//go:linkname freeManual runtime.freeManual
func freeManual(ptr unsafe.Pointer)

func freeString(s string) {
	if len(s) > 0 {
		freeManual(unsafe.Pointer(unsafe.StringData(s)))
	}
}

func freeList(l cm.List[uint8]) {
	if l.Len() > 0 {
		freeManual(unsafe.Pointer(l.Data()))
	}
}

func freeEnvelope(e types.Envelope) {
	freeString(e.ContentType)
	if s := e.Schema.Some(); s != nil {
		freeString(*s)
	}
	freeList(e.Body)
}

func freeExitReason(r types.ExitReason) {
	if s := r.Crashed(); s != nil {
		freeString(*s)
	}
}

func freeInfo(info types.Info) {
	switch {
	case info.Exit() != nil:
		freeExitReason(info.Exit().F1)
	case info.Down() != nil:
		freeExitReason(info.Down().F2)
	case info.Timer() != nil:
		freeEnvelope(info.Timer().F1)
	case info.HostEvent() != nil:
		freeString(info.HostEvent().F0)
		freeEnvelope(info.HostEvent().F1)
	}
}
