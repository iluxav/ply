// benchapi: the REST+Postgres workload for bench/ — one static binary run
// identically under ply and Docker. Three endpoints, nothing clever: the
// runtime around it is what is being measured.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"
)

type User struct {
	ID   int64  `json:"id"`
	Name string `json:"name"`
}

var errNotFound = errors.New("not found")

type Store interface {
	Get(ctx context.Context, id int64) (User, error)
	Insert(ctx context.Context, name string) (int64, error)
	Seed(ctx context.Context, n int) error
}

// dbAddr: ply's `--after pgdb` injects PGDB_ADDR; the Docker side passes
// DB_ADDR explicitly. Empty means no database (only /ping works).
func dbAddr(env map[string]string) string {
	if a := env["PGDB_ADDR"]; a != "" {
		return a
	}
	return env["DB_ADDR"]
}

func newMux(store Store) http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /ping", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprintln(w, "pong")
	})
	mux.HandleFunc("GET /users/{id}", func(w http.ResponseWriter, r *http.Request) {
		id, err := strconv.ParseInt(r.PathValue("id"), 10, 64)
		if err != nil {
			http.Error(w, "bad id", http.StatusBadRequest)
			return
		}
		u, err := store.Get(r.Context(), id)
		switch {
		case errors.Is(err, errNotFound):
			http.Error(w, "no such user", http.StatusNotFound)
		case err != nil:
			http.Error(w, err.Error(), http.StatusInternalServerError)
		default:
			writeJSON(w, http.StatusOK, u)
		}
	})
	mux.HandleFunc("POST /users", func(w http.ResponseWriter, r *http.Request) {
		var in struct {
			Name string `json:"name"`
		}
		if err := json.NewDecoder(r.Body).Decode(&in); err != nil || in.Name == "" {
			http.Error(w, "want {\"name\": ...}", http.StatusBadRequest)
			return
		}
		id, err := store.Insert(r.Context(), in.Name)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		writeJSON(w, http.StatusCreated, User{ID: id, Name: in.Name})
	})
	mux.HandleFunc("POST /seed", func(w http.ResponseWriter, r *http.Request) {
		n, _ := strconv.Atoi(r.URL.Query().Get("n"))
		if n <= 0 {
			n = 10000
		}
		if err := store.Seed(r.Context(), n); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		fmt.Fprintf(w, "seeded %d\n", n)
	})
	return mux
}

func writeJSON(w http.ResponseWriter, code int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	_ = json.NewEncoder(w).Encode(v)
}

// --- Postgres ---------------------------------------------------------------

type pgStore struct{ pool *pgxpool.Pool }

func (s *pgStore) Get(ctx context.Context, id int64) (User, error) {
	var u User
	err := s.pool.QueryRow(ctx, "SELECT id, name FROM users WHERE id = $1", id).Scan(&u.ID, &u.Name)
	if errors.Is(err, pgx.ErrNoRows) {
		return User{}, errNotFound
	}
	return u, err
}

func (s *pgStore) Insert(ctx context.Context, name string) (int64, error) {
	var id int64
	err := s.pool.QueryRow(ctx, "INSERT INTO users (name) VALUES ($1) RETURNING id", name).Scan(&id)
	return id, err
}

// One statement per Exec: the extended protocol refuses several commands
// in a parameterized statement.
func (s *pgStore) Seed(ctx context.Context, n int) error {
	for _, q := range []string{
		"CREATE TABLE IF NOT EXISTS users (id bigserial PRIMARY KEY, name text NOT NULL)",
		"TRUNCATE users RESTART IDENTITY",
	} {
		if _, err := s.pool.Exec(ctx, q); err != nil {
			return err
		}
	}
	_, err := s.pool.Exec(ctx, "INSERT INTO users (name) SELECT 'user-' || g FROM generate_series(1, $1::int) AS g", n)
	return err
}

// connect retries for up to a minute: the database is starting alongside.
func connect(addr string) (*pgxpool.Pool, error) {
	url := fmt.Sprintf("postgres://postgres@%s/postgres?sslmode=disable&pool_max_conns=32", addr)
	deadline := time.Now().Add(60 * time.Second)
	for {
		pool, err := pgxpool.New(context.Background(), url)
		if err == nil {
			ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
			err = pool.Ping(ctx)
			cancel()
			if err == nil {
				return pool, nil
			}
			pool.Close()
		}
		if time.Now().After(deadline) {
			return nil, err
		}
		time.Sleep(500 * time.Millisecond)
	}
}

func environ() map[string]string {
	m := map[string]string{}
	for _, kv := range os.Environ() {
		if k, v, ok := strings.Cut(kv, "="); ok {
			m[k] = v
		}
	}
	return m
}

func main() {
	var store Store
	addr := dbAddr(environ())
	if addr != "" {
		pool, err := connect(addr)
		if err != nil {
			log.Fatalf("benchapi: database %s: %v", addr, err)
		}
		store = &pgStore{pool: pool}
	}
	log.SetFlags(0)
	log.Printf("benchapi listening on :8080 (db %q)", addr)
	stop := make(chan struct{})
	go func() {
		sig := make(chan os.Signal, 1)
		signal.Notify(sig, syscall.SIGTERM, syscall.SIGINT)
		<-sig
		close(stop)
	}()
	if err := serve(":8080", newMux(store), stop); err != nil {
		log.Fatal(err)
	}
}

// closeWhenDraining marks every response `Connection: close` once `closing`
// is set: the client hangs up after reading it and reconnects — to whatever
// the runtime routes it to now. Nothing is closed under a request already
// on the wire, which is what `SetKeepAlivesEnabled(false)` and `Shutdown`
// both do to idle connections (and under load "idle" is that instant).
func closeWhenDraining(closing *atomic.Bool, h http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if closing.Load() {
			w.Header().Set("Connection", "close")
		}
		h.ServeHTTP(w, r)
	})
}

// serve runs until `stop` closes, then drains: responses ask clients to
// close, the open-connection count is watched down to zero (or 3 s), and
// only then does Shutdown run — with nothing left for it to cut.
func serve(addr string, h http.Handler, stop <-chan struct{}) error {
	var closing atomic.Bool
	var open atomic.Int64
	srv := &http.Server{
		Addr:    addr,
		Handler: closeWhenDraining(&closing, h),
		ConnState: func(_ net.Conn, s http.ConnState) {
			switch s {
			case http.StateNew:
				open.Add(1)
			case http.StateClosed, http.StateHijacked:
				open.Add(-1)
			}
		},
	}
	errc := make(chan error, 1)
	go func() {
		if err := srv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			errc <- err
		}
		close(errc)
	}()
	select {
	case err := <-errc:
		return err
	case <-stop:
	}
	closing.Store(true)
	deadline := time.Now().Add(3 * time.Second)
	for open.Load() > 0 && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := srv.Shutdown(ctx); err != nil {
		return err
	}
	return <-errc
}
