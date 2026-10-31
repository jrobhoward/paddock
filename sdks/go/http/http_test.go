package http

import (
	"bytes"
	"reflect"
	"testing"

	bulkhead "github.com/jrobhoward/bulkhead/sdks/go"
	"github.com/jrobhoward/bulkhead/sdks/go/internal/formats"
)

func example(t *testing.T, name string) []byte {
	t.Helper()
	b, err := formats.Example("cbor-hex", name)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

func documentedRequest() Request {
	return Request{
		Scheme: "http", Method: "POST", Path: "/board/inc",
		Query: "by=2", HasQuery: true,
		Headers: []Header{{"host", "localhost:8080"}, {"content-type", "text/plain"}},
		Body:    []byte("hi"),
		Key:     "tenant-7", HasKey: true,
	}
}

func TestRequest_DocumentedExample_ReadWithEveryFieldAndWrittenAlike(t *testing.T) {
	documented := example(t, "http-request")
	msg := bulkhead.Envelope{ContentType: bulkhead.CBORType, Schema: RequestSchema, Body: documented}

	read, ok := RequestFrom(msg)
	written := documentedRequest().Envelope()

	if !ok || !reflect.DeepEqual(read, documentedRequest()) {
		t.Errorf("read %+v %v", read, ok)
	}
	if !bytes.Equal(written.Body, documented) || written.Schema != RequestSchema {
		t.Errorf("written %x", written.Body)
	}
}

func TestResponse_DocumentedExample_SameBytesAndReadBack(t *testing.T) {
	documented := example(t, "http-response")

	msg := NewResponse(201).Header("content-type", "text/plain").WithBody([]byte("ok")).Envelope()
	read, ok := ResponseFrom(msg)

	if !bytes.Equal(msg.Body, documented) || msg.ContentType != bulkhead.CBORType || msg.Schema != ResponseSchema {
		t.Errorf("written %x %q %q", msg.Body, msg.ContentType, msg.Schema)
	}
	if !ok || read.Status != 201 || string(read.Body) != "ok" {
		t.Errorf("read %+v %v", read, ok)
	}
}

func TestRequest_HelpersAndOtherSchemas(t *testing.T) {
	req := documentedRequest()

	host, _ := req.Header("Host")
	by, _ := req.Param("by")
	_, missing := req.Param("nope")
	_, plain := RequestFrom(bulkhead.Text("GET /"))

	if host != "localhost:8080" || by != "2" || missing || plain {
		t.Errorf("%q %q %v %v", host, by, missing, plain)
	}
}
