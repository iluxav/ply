package main

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// fakeStore stands in for Postgres: the handlers are what is under test.
type fakeStore struct {
	users  map[int64]string
	seeded int
}

func (f *fakeStore) Get(_ context.Context, id int64) (User, error) {
	name, ok := f.users[id]
	if !ok {
		return User{}, errNotFound
	}
	return User{ID: id, Name: name}, nil
}

func (f *fakeStore) Insert(_ context.Context, name string) (int64, error) {
	id := int64(len(f.users) + 1)
	f.users[id] = name
	return id, nil
}

func (f *fakeStore) Seed(_ context.Context, n int) error {
	f.seeded = n
	return nil
}

func newFake() *fakeStore { return &fakeStore{users: map[int64]string{42: "alice"}} }

func do(h http.Handler, method, path, body string) *httptest.ResponseRecorder {
	req := httptest.NewRequest(method, path, strings.NewReader(body))
	if body != "" {
		req.Header.Set("Content-Type", "application/json")
	}
	rr := httptest.NewRecorder()
	h.ServeHTTP(rr, req)
	return rr
}

func TestPingNeverTouchesTheStore(t *testing.T) {
	rr := do(newMux(nil), "GET", "/ping", "")
	if rr.Code != 200 || strings.TrimSpace(rr.Body.String()) != "pong" {
		t.Fatalf("got %d %q", rr.Code, rr.Body.String())
	}
}

func TestReadReturnsTheUserAsJSON(t *testing.T) {
	rr := do(newMux(newFake()), "GET", "/users/42", "")
	if rr.Code != 200 {
		t.Fatalf("got %d %q", rr.Code, rr.Body.String())
	}
	var u User
	if err := json.Unmarshal(rr.Body.Bytes(), &u); err != nil || u.ID != 42 || u.Name != "alice" {
		t.Fatalf("got %+v (%v)", u, err)
	}
}

func TestReadOfMissingUserIs404AndBadIdIs400(t *testing.T) {
	if rr := do(newMux(newFake()), "GET", "/users/7", ""); rr.Code != 404 {
		t.Fatalf("missing: got %d", rr.Code)
	}
	if rr := do(newMux(newFake()), "GET", "/users/abc", ""); rr.Code != 400 {
		t.Fatalf("bad id: got %d", rr.Code)
	}
}

func TestInsertReturns201WithTheNewId(t *testing.T) {
	f := newFake()
	rr := do(newMux(f), "POST", "/users", `{"name":"bob"}`)
	if rr.Code != 201 {
		t.Fatalf("got %d %q", rr.Code, rr.Body.String())
	}
	var u User
	if err := json.Unmarshal(rr.Body.Bytes(), &u); err != nil || u.ID != 2 || u.Name != "bob" {
		t.Fatalf("got %+v (%v)", u, err)
	}
	if f.users[2] != "bob" {
		t.Fatalf("store not written: %v", f.users)
	}
}

func TestSeedCreatesNRows(t *testing.T) {
	f := newFake()
	rr := do(newMux(f), "POST", "/seed?n=500", "")
	if rr.Code != 200 || f.seeded != 500 {
		t.Fatalf("got %d, seeded %d", rr.Code, f.seeded)
	}
}

func TestDbAddrPrefersPlyInjectionThenDockerEnv(t *testing.T) {
	if got := dbAddr(map[string]string{"PGDB_ADDR": "10.77.0.1:5432", "DB_ADDR": "pgdb:5432"}); got != "10.77.0.1:5432" {
		t.Fatalf("got %q", got)
	}
	if got := dbAddr(map[string]string{"DB_ADDR": "pgdb:5432"}); got != "pgdb:5432" {
		t.Fatalf("got %q", got)
	}
	if got := dbAddr(map[string]string{}); got != "" {
		t.Fatalf("got %q", got)
	}
}

// A rolling restart is only as clean as the app: on the stop signal the
// server must stop accepting, finish what is in flight, and return.
func TestServeStopsCleanlyWhenToldTo(t *testing.T) {
	stop := make(chan struct{})
	done := make(chan error, 1)
	go func() { done <- serve("127.0.0.1:0", newMux(nil), stop) }()
	close(stop)
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("serve returned %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("serve did not return after stop")
	}
}

// Draining without racing the client: once closing, every response says
// `Connection: close`, so the CLIENT ends the connection after reading it —
// nothing is closed under a request already on the wire.
func TestWhileClosingEveryResponseAsksTheClientToClose(t *testing.T) {
	var closing atomic.Bool
	h := closeWhenDraining(&closing, newMux(nil))
	if rr := do(h, "GET", "/ping", ""); rr.Header().Get("Connection") != "" {
		t.Fatalf("before closing: %q", rr.Header().Get("Connection"))
	}
	closing.Store(true)
	if rr := do(h, "GET", "/ping", ""); rr.Header().Get("Connection") != "close" || rr.Code != 200 {
		t.Fatalf("while closing: %d %q", rr.Code, rr.Header().Get("Connection"))
	}
}
