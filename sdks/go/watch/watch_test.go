package watch

import (
	"testing"

	paddock "github.com/jrobhoward/paddock/sdks/go"
	"github.com/jrobhoward/paddock/sdks/go/internal/formats"
)

func TestChangeFromInfo_DocumentedExample_ReadAsAChange(t *testing.T) {
	body, err := formats.Example("json", "kv-changed")
	if err != nil {
		t.Fatal(err)
	}
	info := paddock.EventInfo{Name: Event, Msg: paddock.Envelope{ContentType: paddock.JSONType, Body: body}}

	change, ok := ChangeFromInfo(info)
	_, other := ChangeFromInfo(paddock.EventInfo{Name: "log-level", Msg: paddock.Text("info")})

	want := Change{Watch: 1, Bucket: "board", Key: "count", Revision: 7}
	if !ok || change != want {
		t.Errorf("got %+v %v", change, ok)
	}
	if other {
		t.Error("another event read as a change")
	}
}
