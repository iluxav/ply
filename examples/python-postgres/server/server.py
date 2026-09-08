"""A guestbook: every request records a visit in Postgres and reports the count.

DATABASE_URL arrives from the stack file ({db.url}); PORT is set by ply when it
hands the app a port (rootless runs), 8000 otherwise.
"""
import os
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

import psycopg2

DATABASE_URL = os.environ["DATABASE_URL"]
PORT = int(os.environ.get("PORT", 8000))


def query(sql, *params):
    """One connection per request: nothing to keep alive, nothing to go stale."""
    conn = psycopg2.connect(DATABASE_URL)
    try:
        with conn, conn.cursor() as cur:
            cur.execute(sql, params)
            return cur.fetchall() if cur.description else None
    finally:
        conn.close()


# The stack starts this app only after db is healthy, but a first boot can
# still be settling; retry briefly rather than exit and let ply restart us.
for attempt in range(30):
    try:
        query("CREATE TABLE IF NOT EXISTS visits (id serial PRIMARY KEY, at timestamptz NOT NULL DEFAULT now())")
        break
    except psycopg2.OperationalError as e:
        print(f"waiting for postgres ({e.args[0].strip().splitlines()[0]})")
        time.sleep(1)
else:
    raise SystemExit("postgres did not answer")


class Guestbook(BaseHTTPRequestHandler):
    def do_GET(self):
        query("INSERT INTO visits DEFAULT VALUES")
        [(count,)] = query("SELECT count(*) FROM visits")
        body = f"Hello from Python + Postgres! visit #{count}\n".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        print(f"{self.address_string()} {fmt % args}")


print(f"guestbook listening on {PORT}")
HTTPServer(("0.0.0.0", PORT), Guestbook).serve_forever()
