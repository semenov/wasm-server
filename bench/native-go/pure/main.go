// The notes app as a native Go server (SQLite: pure-Go modernc.org/sqlite),
// for comparison with the wasm apps. Usage: gonative <db file> <listen addr>
package main

import (
	"database/sql"
	"encoding/json"
	"io"
	"log"
	"net/http"
	"os"
	"sync/atomic"
	"time"

	_ "modernc.org/sqlite"
)

type note struct {
	ID   int64  `json:"id"`
	Text string `json:"text"`
	At   int64  `json:"at"`
}

func main() {
	db, err := sql.Open("sqlite", os.Args[1]+"?_pragma=journal_mode(WAL)&_pragma=synchronous(NORMAL)&_pragma=busy_timeout(5000)")
	if err != nil {
		log.Fatal(err)
	}
	db.SetMaxOpenConns(16)
	if _, err := db.Exec(`CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, text TEXT NOT NULL, at INTEGER NOT NULL)`); err != nil {
		log.Fatal(err)
	}
	var views atomic.Int64
	reply := func(w http.ResponseWriter, status int, v any) {
		w.Header().Set("content-type", "application/json")
		w.WriteHeader(status)
		json.NewEncoder(w).Encode(v)
	}
	http.HandleFunc("GET /gonotes", func(w http.ResponseWriter, r *http.Request) {
		v := views.Add(1)
		rows, err := db.Query(`SELECT id, text, at FROM notes ORDER BY id DESC LIMIT 20`)
		if err != nil {
			reply(w, 500, map[string]string{"error": err.Error()})
			return
		}
		defer rows.Close()
		notes := []note{}
		for rows.Next() {
			var n note
			rows.Scan(&n.ID, &n.Text, &n.At)
			notes = append(notes, n)
		}
		reply(w, 200, map[string]any{"views": v, "notes": notes})
	})
	http.HandleFunc("POST /gonotes", func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(io.LimitReader(r.Body, 16*1024+1))
		switch {
		case err != nil || len(body) > 16*1024:
			reply(w, 413, map[string]string{"error": "note too long"})
			return
		case len(body) == 0:
			reply(w, 422, map[string]string{"error": "empty note"})
			return
		}
		var id int64
		if err := db.QueryRow(`INSERT INTO notes (text, at) VALUES (?, ?) RETURNING id`, string(body), time.Now().Unix()).Scan(&id); err != nil {
			reply(w, 500, map[string]string{"error": err.Error()})
			return
		}
		reply(w, 201, map[string]int64{"id": id})
	})
	log.Fatal(http.ListenAndServe(os.Args[2], nil))
}
