#!/bin/sh
# Surgical restore: gunzip a dump from stdin into a database, over the
# local socket. Made for `ply exec` (which forwards stdin):
#
#   <fetch dump.sql.gz> | ply exec <db>.1 -- ./restore.sh --to side_db
#
# --to NAME   restore into NAME (created if missing) instead of the live
#             POSTGRES_DB — inspect yesterday's data next to today's.
# --replace NAME  restore OVER the live database NAME: every connection to
#             it is terminated, it is dropped and recreated, the dump loads.
#             The app reconnects (a database client retries); what it wrote
#             since the dump is gone, which is what "restore" means. The
#             name is required and never guessed: a guess that landed on the
#             wrong database would be the one unforgivable outcome.
set -eu
# exec sessions compose neither PATH nor LD_LIBRARY_PATH from layers —
# this script carries its own, arch-aware
PGROOT=$(echo /opt/postgresql17-*)
PGBIN="$PGROOT/usr/lib/postgresql/17/bin"
export LD_LIBRARY_PATH="$PGROOT/usr/lib/$(uname -m)-linux-gnu:$PGROOT/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
SOCK=/tmp
# the socket file carries the server's port in its name — discover it
# instead of guessing (exec sessions can't read the entrypoint's PGPORT)
PORT=$(ls "$SOCK"/.s.PGSQL.* 2>/dev/null | grep -v '\.lock$' | head -1 | sed 's/.*\.//')
PORT="${PORT:-5432}"

TARGET=""
REPLACE=""
[ "${1:-}" = "--to" ] && TARGET="${2:?--to needs a database name}"
[ "${1:-}" = "--replace" ] && REPLACE="${2:?--replace needs the live database's name (POSTGRES_DB)}"
# `ply exec` runs a command with the app's own environment, so
# POSTGRES_USER is normally right here; default to the image's default.
USER="${POSTGRES_USER:-postgres}"

if [ -n "$REPLACE" ]; then
  LIVE="$REPLACE"
  if [ "$LIVE" = postgres ] || [ "$LIVE" = template0 ] || [ "$LIVE" = template1 ]; then
    echo "restore.sh: refusing to replace $LIVE — that is a system database, not the app's" >&2
    exit 2
  fi
  echo "restore.sh: replacing the live database $LIVE — connections to it are being terminated" >&2
  "$PGBIN/psql" -h "$SOCK" -p "$PORT" -U "$USER" -d postgres -q \
    -c "DROP DATABASE IF EXISTS \"$LIVE\" WITH (FORCE)" \
    -c "CREATE DATABASE \"$LIVE\""
  gunzip | "$PGBIN/psql" -h "$SOCK" -p "$PORT" -U "$USER" -q -d "$LIVE"
  echo "restore.sh: restored over $LIVE"
  exit 0
fi
if [ -z "$TARGET" ]; then
  echo "restore.sh: refusing to restore into the live database without --to NAME (or --replace)" >&2
  echo "  (a restore over live data is a deliberate act: name it)" >&2
  exit 2
fi
$PGBIN/psql -h "$SOCK" -p "$PORT" -U "$USER" -tAc "SELECT 1 FROM pg_database WHERE datname='$TARGET'" | grep -q 1 \
  || $PGBIN/psql -h "$SOCK" -p "$PORT" -U "$USER" -c "CREATE DATABASE \"$TARGET\"" >/dev/null
gunzip | $PGBIN/psql -h "$SOCK" -p "$PORT" -U "$USER" -q -d "$TARGET"
echo "restore.sh: restored into $TARGET"
