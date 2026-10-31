// Package http is HTTP requests from the node's ingress, which arrive as
// calls, and their responses, in GUEST_FORMATS.md's formats.
//
// A route in the node's HTTP config sends each request on its paths to the
// app as a call with schema `paddock/http-request`. Answer with a
// [Response] envelope, now or later:
//
//	func (a *App) HandleCall(ctx *paddock.Ctx, call paddock.CallRef, msg paddock.Envelope) paddock.Reply {
//		if req, ok := http.RequestFrom(msg); ok {
//			return paddock.Now(http.Text(200, "hello "+req.Path).Envelope())
//		}
//		...
//	}
package http

import (
	"strings"
	"unicode/utf8"

	paddock "github.com/jrobhoward/paddock/sdks/go"
)

// Schemas of a request and a response.
const (
	RequestSchema  = "paddock/http-request"
	ResponseSchema = "paddock/http-response"
)

// Header is one header: a lower-case name and its value.
type Header struct {
	Name  string
	Value string
}

// Request is an HTTP request from the ingress.
type Request struct {
	// Scheme is http or https.
	Scheme string
	// Method is as sent: GET, POST ...
	Method string
	// Path is without the query.
	Path string
	// Query is after the ?, without it; HasQuery is false when there is none.
	Query    string
	HasQuery bool
	// Headers in the order received, names in lower case; a repeated
	// header appears once per value.
	Headers []Header
	// Body is up to the route's max_body.
	Body []byte
	// Key is the route's hash_header value, for hash:key routing; HasKey
	// is false when the route has none.
	Key    string
	HasKey bool
}

type wireRequest struct {
	Scheme  string     `cbor:"scheme"`
	Method  string     `cbor:"method"`
	Path    string     `cbor:"path"`
	Query   *string    `cbor:"query"`
	Headers [][]string `cbor:"headers"`
	Body    []byte     `cbor:"body"`
	Key     *string    `cbor:"key,omitempty"`
}

type wireResponse struct {
	Status  uint16     `cbor:"status"`
	Headers [][]string `cbor:"headers"`
	Body    []byte     `cbor:"body"`
}

func headersFrom(wire [][]string) []Header {
	out := make([]Header, 0, len(wire))
	for _, pair := range wire {
		if len(pair) == 2 {
			out = append(out, Header{Name: pair[0], Value: pair[1]})
		}
	}
	return out
}

func headersTo(headers []Header) [][]string {
	out := make([][]string, 0, len(headers))
	for _, h := range headers {
		out = append(out, []string{h.Name, h.Value})
	}
	return out
}

// RequestFrom reads msg as a request, if it is one.
func RequestFrom(msg paddock.Envelope) (Request, bool) {
	if msg.Schema != RequestSchema {
		return Request{}, false
	}
	var w wireRequest
	if paddock.UnmarshalCBOR(msg.Body, &w) != nil || w.Method == "" || w.Path == "" {
		return Request{}, false
	}
	r := Request{
		Scheme:  w.Scheme,
		Method:  w.Method,
		Path:    w.Path,
		Headers: headersFrom(w.Headers),
		Body:    w.Body,
	}
	if r.Scheme == "" {
		r.Scheme = "http"
	}
	if w.Query != nil {
		r.Query, r.HasQuery = *w.Query, true
	}
	if w.Key != nil {
		r.Key, r.HasKey = *w.Key, true
	}
	if r.Body == nil {
		r.Body = []byte{}
	}
	return r, true
}

// Envelope writes the request as the ingress sends it.
func (r Request) Envelope() paddock.Envelope {
	w := wireRequest{
		Scheme:  r.Scheme,
		Method:  r.Method,
		Path:    r.Path,
		Headers: headersTo(r.Headers),
		Body:    r.Body,
	}
	if r.HasQuery {
		w.Query = &r.Query
	}
	if r.HasKey {
		w.Key = &r.Key
	}
	return encode(w, RequestSchema)
}

// Header is the first value of header name, matched without regard to
// case.
func (r Request) Header(name string) (string, bool) {
	for _, h := range r.Headers {
		if strings.EqualFold(h.Name, name) {
			return h.Value, true
		}
	}
	return "", false
}

// Text is the body, if it is valid UTF-8.
func (r Request) Text() (string, bool) {
	if !utf8.Valid(r.Body) {
		return "", false
	}
	return string(r.Body), true
}

// Param is the first value of query parameter name, not percent-decoded.
func (r Request) Param(name string) (string, bool) {
	if !r.HasQuery {
		return "", false
	}
	for _, kv := range strings.Split(r.Query, "&") {
		k, v, _ := strings.Cut(kv, "=")
		if k == name {
			return v, true
		}
	}
	return "", false
}

// Response is the answer to a request.
type Response struct {
	Status  uint16
	Headers []Header
	Body    []byte
}

// NewResponse is an empty response with status.
func NewResponse(status uint16) Response {
	return Response{Status: status}
}

// Text is a text/plain response.
func Text(status uint16, body string) Response {
	return NewResponse(status).Header("content-type", "text/plain; charset=utf-8").WithBody([]byte(body))
}

// JSON is an application/json response with v encoded.
func JSON(status uint16, v any) (Response, error) {
	body, err := paddock.MarshalJSON(v)
	if err != nil {
		return Response{}, err
	}
	return NewResponse(status).Header("content-type", "application/json").WithBody(body), nil
}

// Header returns r with a header added.
func (r Response) Header(name, value string) Response {
	r.Headers = append(append([]Header(nil), r.Headers...), Header{Name: name, Value: value})
	return r
}

// WithBody returns r with its body set.
func (r Response) WithBody(body []byte) Response {
	r.Body = body
	return r
}

// Envelope writes the response for a reply.
func (r Response) Envelope() paddock.Envelope {
	return encode(wireResponse{Status: r.Status, Headers: headersTo(r.Headers), Body: r.Body}, ResponseSchema)
}

// ResponseFrom reads msg as a response, if it is one; a missing status is
// 200.
func ResponseFrom(msg paddock.Envelope) (Response, bool) {
	if msg.Schema != ResponseSchema {
		return Response{}, false
	}
	w := wireResponse{Status: 200}
	if paddock.UnmarshalCBOR(msg.Body, &w) != nil {
		return Response{}, false
	}
	return Response{Status: w.Status, Headers: headersFrom(w.Headers), Body: w.Body}, true
}

func encode(v any, schema string) paddock.Envelope {
	// Encoding these types cannot fail.
	msg, _ := paddock.CBOR(v)
	return msg.WithSchema(schema)
}
