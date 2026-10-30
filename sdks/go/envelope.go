package paddock

import (
	"encoding/json"
	"fmt"
	"strings"
	"unicode/utf8"

	"github.com/fxamacker/cbor/v2"
	"github.com/jrobhoward/paddock/sdks/go/internal/abi"
	"github.com/jrobhoward/paddock/sdks/go/internal/gen/paddock/runtime/types"
)

// Content types Paddock reads.
const (
	TextType = "text/plain"
	JSONType = "application/json"
	CBORType = "application/cbor"
)

// Envelope is a message: a content type, an optional schema (empty for
// none) and a body.
type Envelope struct {
	ContentType string
	Schema      string
	Body        []byte
}

// Text is a text/plain envelope.
func Text(s string) Envelope {
	return Envelope{ContentType: TextType, Body: []byte(s)}
}

// CBOR encodes v as an application/cbor envelope, as GUEST_FORMATS.md
// says Paddock writes CBOR: definite lengths, the shortest integers, a
// struct's fields in their order, and nil slices and maps as empty ones.
func CBOR(v any) (Envelope, error) {
	body, err := MarshalCBOR(v)
	if err != nil {
		return Envelope{}, err
	}
	return Envelope{ContentType: CBORType, Body: body}, nil
}

// JSON encodes v as an application/json envelope.
func JSON(v any) (Envelope, error) {
	body, err := json.Marshal(v)
	if err != nil {
		return Envelope{}, fmt.Errorf("JSON encode: %w", err)
	}
	return Envelope{ContentType: JSONType, Body: body}, nil
}

// WithSchema returns e with its schema set.
func (e Envelope) WithSchema(schema string) Envelope {
	e.Schema = schema
	return e
}

// MediaType is the content type without parameters, in lower case.
func (e Envelope) MediaType() string {
	media, _, _ := strings.Cut(e.ContentType, ";")
	return strings.ToLower(strings.TrimSpace(media))
}

// AsText is the body, if the envelope is text/plain and the body valid
// UTF-8.
func (e Envelope) AsText() (string, bool) {
	if e.MediaType() != TextType || !utf8.Valid(e.Body) {
		return "", false
	}
	return string(e.Body), true
}

// Decode decodes the body into v by its content type: CBOR or JSON.
func (e Envelope) Decode(v any) error {
	switch e.MediaType() {
	case CBORType:
		if err := cbor.Unmarshal(e.Body, v); err != nil {
			return fmt.Errorf("CBOR decode: %w", err)
		}
		return nil
	case JSONType:
		if err := json.Unmarshal(e.Body, v); err != nil {
			return fmt.Errorf("JSON decode: %w", err)
		}
		return nil
	default:
		return fmt.Errorf("cannot decode content type '%s'", e.MediaType())
	}
}

var cborMode = func() cbor.EncMode {
	mode, err := cbor.EncOptions{
		NilContainers: cbor.NilContainerAsEmpty,
		Time:          cbor.TimeRFC3339Nano,
	}.EncMode()
	if err != nil {
		panic(err)
	}
	return mode
}()

// MarshalCBOR encodes v as Paddock writes CBOR (see [CBOR]).
func MarshalCBOR(v any) ([]byte, error) {
	body, err := cborMode.Marshal(v)
	if err != nil {
		return nil, fmt.Errorf("CBOR encode: %w", err)
	}
	return body, nil
}

// UnmarshalCBOR decodes CBOR into v.
func UnmarshalCBOR(data []byte, v any) error {
	if err := cbor.Unmarshal(data, v); err != nil {
		return fmt.Errorf("CBOR decode: %w", err)
	}
	return nil
}

func takeEnvelope(e types.Envelope) Envelope {
	c := abi.TakeEnvelope(e)
	return Envelope{ContentType: c.ContentType, Schema: c.Schema, Body: c.Body}
}

func (e Envelope) host() types.Envelope {
	return abi.HostEnvelope(e.ContentType, e.Schema, e.Body)
}
