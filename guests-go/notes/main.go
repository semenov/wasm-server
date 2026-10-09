// The notes app in Go, built with TinyGo; the same as guests-c/notes and
// guests/notes-rs:
//
//	GET  /gonotes   latest notes and the view count (JSON)
//	POST /gonotes   add a note; the body is its text
//
// Bindings: wit-bindgen-go (go.bytecodealliance.org) into internal/.
package main

import (
	"encoding/json"
	"strings"
	"time"

	"go.bytecodealliance.org/cm"

	"example.com/gonotes/internal/platform/sdk/cache"
	"example.com/gonotes/internal/platform/sdk/sql"
	incominghandler "example.com/gonotes/internal/wasi/http/incoming-handler"
	"example.com/gonotes/internal/wasi/http/types"
)

const (
	db      = "notes"
	maxBody = 16 * 1024
)

type note struct {
	ID   int64  `json:"id"`
	Text string `json:"text"`
	At   int64  `json:"at"`
}

func init() {
	incominghandler.Exports.Handle = handle
}

func handle(request types.IncomingRequest, out types.ResponseOutparam) {
	path := ""
	if p := request.PathWithQuery(); p.Some() != nil {
		path = *p.Some()
	}
	root := path == "/gonotes" || path == "/gonotes/" || strings.HasPrefix(path, "/gonotes?")
	method := request.Method()
	switch {
	case root && method.Get():
		list(out)
	case root && method.Post():
		add(request, out)
	default:
		respond(out, 404, map[string]string{"error": "not found"})
	}
	request.ResourceDrop()
}

func list(out types.ResponseOutparam) {
	views := cache.Incr("views", 1, 0)
	r := sql.Query(db, "SELECT id, text, at FROM notes ORDER BY id DESC LIMIT 20", cm.List[sql.Value]{})
	if r.IsErr() {
		respond(out, 500, map[string]string{"error": sqlError(r.Err())})
		return
	}
	notes := []note{}
	for _, row := range r.OK().Rows.Slice() {
		cols := row.Slice()
		notes = append(notes, note{ID: *cols[0].Int(), Text: *cols[1].Text(), At: *cols[2].Int()})
	}
	respond(out, 200, map[string]any{"views": views, "notes": notes})
}

func add(request types.IncomingRequest, out types.ResponseOutparam) {
	text, ok := readBody(request)
	if !ok {
		respond(out, 413, map[string]string{"error": "note too long"})
		return
	}
	if len(text) == 0 {
		respond(out, 422, map[string]string{"error": "empty note"})
		return
	}
	params := cm.ToList([]sql.Value{sql.ValueText(string(text)), sql.ValueInt(time.Now().Unix())})
	r := sql.Query(db, "INSERT INTO notes (text, at) VALUES ($1, $2) RETURNING id", params)
	if r.IsErr() {
		respond(out, 500, map[string]string{"error": sqlError(r.Err())})
		return
	}
	id := r.OK().Rows.Slice()[0].Slice()[0].Int()
	respond(out, 201, map[string]int64{"id": *id})
}

func sqlError(e *sql.Error) string {
	for _, s := range []*string{e.NotAllowed(), e.Unavailable(), e.Query(), e.Conversion()} {
		if s != nil {
			return *s
		}
	}
	return "database error"
}

// readBody returns the request body, or false if it's larger than maxBody.
func readBody(request types.IncomingRequest) ([]byte, bool) {
	consumed := request.Consume()
	if consumed.IsErr() {
		return nil, false
	}
	body := *consumed.OK()
	streamed := body.Stream()
	if streamed.IsErr() {
		return nil, false
	}
	stream := *streamed.OK()
	var data []byte
	ok := true
	for {
		chunk := stream.BlockingRead(65536)
		if chunk.IsErr() {
			break // closed: end of body
		}
		bytes := chunk.OK().Slice()
		if len(data)+len(bytes) > maxBody {
			ok = false
			break
		}
		data = append(data, bytes...)
	}
	stream.ResourceDrop()
	types.IncomingBodyFinish(body).ResourceDrop()
	return data, ok
}

func respond(out types.ResponseOutparam, status types.StatusCode, value any) {
	payload, err := json.Marshal(value)
	if err != nil {
		status, payload = 500, []byte(`{"error":"encoding failed"}`)
	}
	header := cm.Tuple[types.FieldName, types.FieldValue]{F0: "content-type", F1: types.FieldValue(cm.ToList([]byte("application/json")))}
	fields := types.FieldsFromList(cm.ToList([]cm.Tuple[types.FieldName, types.FieldValue]{header}))
	resp := types.NewOutgoingResponse(*fields.OK())
	resp.SetStatusCode(status)
	outBody := resp.Body()
	body := *outBody.OK()
	types.ResponseOutparamSet(out, cm.OK[cm.Result[types.ErrorCodeShape, types.OutgoingResponse, types.ErrorCode]](resp))

	written := body.Write()
	stream := *written.OK()
	// blocking-write-and-flush takes at most 4096 bytes at a time.
	for len(payload) > 0 {
		n := min(len(payload), 4096)
		result := stream.BlockingWriteAndFlush(cm.ToList(payload[:n]))
		if result.IsErr() {
			break
		}
		payload = payload[n:]
	}
	stream.ResourceDrop()
	types.OutgoingBodyFinish(body, cm.None[types.Trailers]())
}

func main() {}
