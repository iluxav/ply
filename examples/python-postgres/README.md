# python-postgres

A two-service [composition](https://plybox.sh/docs/stacks/): the registry's
prebuilt Postgres and a Python HTTP server that records every visit in it.

From this directory:

```sh
ply up
curl http://127.0.0.1:8080        # in another terminal on the same host
# Hello from Python + Postgres! visit #1
```

`ply up` reads `ply.toml`, starts `postgres@17` with a `guestbook` database
and a password it mints and stores under `.ply/secrets/`, waits for it to
answer on 5432, then builds `server/` and starts it with `DATABASE_URL`
filled in from the database's own parameters. Ctrl-C stops both. Run it
again and the count continues: the data lives in a ply-managed volume.

What the two manifests say:

- `ply.toml` is wiring only — a `[package]` header plus one `[[service]]`
  per `ply run`. The `{db.url}` reference is the connection string *and* the
  start order; there is no separate `after`.
- `server/ply.toml` is an ordinary app: `python3` and `python3-psycopg2`
  from the registry on a `debian@13` base, a `[health]` port so the stack
  and `ply deploy` know when it is ready. psycopg2 lives in its own keg
  under `/opt/python3-psycopg2-2.9.10`, so `PYTHONPATH` names it; the
  dependency is pinned exactly so that path cannot drift.

`server/server.py` is the standard library's `http.server` plus psycopg2:
one connection per request, a `CREATE TABLE IF NOT EXISTS` at start, and a
short retry loop in case the database is still settling on its first boot.

The minted password and the database's data are two files that must stay
together: `.ply/secrets/db.password` here, and the `db` volume under
`~/.local/share/ply/volumes/` (`/var/lib/ply/volumes/` rootful). Postgres
reads the password on its first boot only, so deleting `.ply/` alone mints a
new one that the existing data refuses. To start over, remove both.

Useful while it runs, from another terminal:

```sh
ply ps                            # both members, their addresses and ports
ply logs server                   # the server's output
ply why server                    # exits, restarts, blocked traffic — if something is wrong
```
