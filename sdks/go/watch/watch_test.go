package watch

import (
	"testing"

	bulkhead "github.com/jrobhoward/bulkhead/sdks/go"
	"github.com/jrobhoward/bulkhead/sdks/go/internal/formats"
)

func TestChangeFromInfo_DocumentedExample_ReadAsAChange(t *testing.T) {
	body, err := formats.Example("json", "kv-changed")
	if err != nil {
		t.Fatal(err)
	}
	info := bulkhead.EventInfo{Name: Event, Msg: bulkhead.Envelope{ContentType: bulkhead.JSONType, Body: body}}

	change, ok := ChangeFromInfo(info)
	_, other := ChangeFromInfo(bulkhead.EventInfo{Name: "log-level", Msg: bulkhead.Text("info")})

	want := Change{Watch: 1, Bucket: "board", Key: "count", Revision: 7}
	if !ok || change != want {
		t.Errorf("got %+v %v", change, ok)
	}
	if other {
		t.Error("another event read as a change")
	}
}
