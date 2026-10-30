// Package abi copies what the host passes into Go memory and frees the
// originals.
//
// TinyGo's cabi_realloc allocates with allocManual: a block stays live
// until it is freed. The host lowers every string and list it passes to an
// export, and every string and list an import returns, through it, and the
// glue wit-bindgen-go generates never frees them, so without this every
// message leaks until the instance reaches memory_max (spike S21). Each
// function here copies a value out and frees the block it came in.
package abi

import (
	"strings"
	"unsafe"

	"github.com/jrobhoward/paddock/sdks/go/internal/gen/paddock/runtime/types"
	"go.bytecodealliance.org/cm"
)

//go:linkname freeManual runtime.freeManual
func freeManual(ptr unsafe.Pointer)

// String copies s and frees the host's block.
func String(s string) string {
	if len(s) == 0 {
		return ""
	}
	out := strings.Clone(s)
	freeManual(unsafe.Pointer(unsafe.StringData(s)))
	return out
}

// Bytes copies l and frees the host's block.
func Bytes(l cm.List[uint8]) []byte {
	if l.Len() == 0 {
		return []byte{}
	}
	out := make([]byte, l.Len())
	copy(out, l.Slice())
	freeManual(unsafe.Pointer(l.Data()))
	return out
}

// Strings copies each string of l, then frees them and l.
func Strings(l cm.List[string]) []string {
	out := make([]string, 0, l.Len())
	for _, s := range l.Slice() {
		out = append(out, String(s))
	}
	FreeList(l)
	return out
}

// FreeList frees the block of a list whose items own no memory, once they
// have been copied.
func FreeList[T any](l cm.List[T]) {
	if l.Len() > 0 {
		freeManual(unsafe.Pointer(l.Data()))
	}
}

// Envelope is an envelope's fields copied out of a host envelope.
type Envelope struct {
	ContentType string
	Schema      string
	Body        []byte
}

// TakeEnvelope copies e and frees its blocks.
func TakeEnvelope(e types.Envelope) Envelope {
	out := Envelope{ContentType: String(e.ContentType), Body: Bytes(e.Body)}
	if s := e.Schema.Some(); s != nil {
		out.Schema = String(*s)
	}
	return out
}

// HostEnvelope builds a host envelope from Go memory; an empty schema is
// none.
func HostEnvelope(contentType, schema string, body []byte) types.Envelope {
	e := types.Envelope{ContentType: contentType, Body: cm.ToList(body)}
	if schema != "" {
		e.Schema = cm.Some(schema)
	}
	return e
}

// TakeCrashed copies the text of a `crashed` exit reason and frees it; other
// reasons own no memory.
func TakeCrashed(r *types.ExitReason) string {
	if s := r.Crashed(); s != nil {
		return String(*s)
	}
	return ""
}
