package bulkhead

import (
	"bytes"
	"testing"

	"github.com/jrobhoward/bulkhead/sdks/go/internal/formats"
)

type board struct {
	Count uint64 `cbor:"count"`
	Label string `cbor:"label"`
}

func TestSnapshot_DocumentedExample_WrittenAndReadAlike(t *testing.T) {
	documented, err := formats.Example("cbor-hex", "snapshot")
	if err != nil {
		t.Fatal(err)
	}

	written, err := EncodeSnapshot(2, board{Count: 5, Label: "board"})
	var read board
	loadErr := LoadSnapshot(documented, 2, &read)

	if err != nil || !bytes.Equal(written, documented) {
		t.Errorf("written %x (%v), documented %x", written, err, documented)
	}
	if loadErr != nil || read != (board{Count: 5, Label: "board"}) {
		t.Errorf("read %+v (%v)", read, loadErr)
	}
}

func TestSnapshot_OtherVersion_Refused(t *testing.T) {
	documented, _ := formats.Example("cbor-hex", "snapshot")

	err := LoadSnapshot(documented, 3, &board{})

	if err == nil || err.Error() != "cannot read snapshot format 2 (this version writes format 3)" {
		t.Errorf("got %v", err)
	}
}

func TestEnvelope_TextAndDecode_ByContentType(t *testing.T) {
	plain := Envelope{ContentType: "Text/Plain; charset=utf-8", Body: []byte("hi")}
	binary := Envelope{ContentType: TextType, Body: []byte{0xff}}
	js := Envelope{ContentType: JSONType, Body: []byte(`{"count":5,"label":"board"}`)}

	text, isText := plain.AsText()
	_, binaryIsText := binary.AsText()
	var fromJSON board
	jsErr := js.Decode(&fromJSON)
	plainErr := plain.Decode(&fromJSON)

	if text != "hi" || !isText || binaryIsText {
		t.Errorf("AsText: %q %v %v", text, isText, binaryIsText)
	}
	if jsErr != nil || fromJSON != (board{Count: 5, Label: "board"}) {
		t.Errorf("Decode JSON: %+v %v", fromJSON, jsErr)
	}
	if plainErr == nil {
		t.Error("Decode text/plain: no error")
	}
}

func TestCBOR_NilSlice_EncodedEmpty(t *testing.T) {
	type kept struct {
		Notes []string `cbor:"notes"`
	}

	body, err := MarshalCBOR(kept{})

	// {"notes": []}
	if err != nil || !bytes.Equal(body, []byte{0xa1, 0x65, 'n', 'o', 't', 'e', 's', 0x80}) {
		t.Errorf("%x %v", body, err)
	}
}

func TestJSON_PidLikeText_NotEscapedForHTML(t *testing.T) {
	body, err := MarshalJSON(map[string]string{"pid": "<1.1.1@1>"})

	if err != nil || string(body) != `{"pid":"<1.1.1@1>"}` {
		t.Errorf("%s %v", body, err)
	}
}
