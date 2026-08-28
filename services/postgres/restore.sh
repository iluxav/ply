#!/bin/sh
# Surgical restore: gunzip a dump from stdin into a database, over the
# local socket. Made for `ply exec` (which forwards stdin):
#
#   <fetch dump.sql.gz> | ply exec <db>.1 -- ./restore.sh --to side_db
#
# --to NAME   restore into NAME (created if missing) instead of the live
#             POSTGRES_DB — inspect yesterday's data next to today's.
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
[ "${1:-}" = "--to" ] && TARGET="${2:?--to needs a database name}"
# exec sessions arrive without the app's env; PID 1 is our entrypoint,
# so borrow its POSTGRES_USER when readable, default otherwise
USER=$({ tr '\0' '\n' < /proc/1/environ; } 2>/dev/null | sed -n 's/^POSTGRES_USER=//p' || true)
USER="${USER:-postgres}"

if [ -z "$TARGET" ]; then
  echo "restore.sh: refusing to restore into the live database without --to NAME" >&2
  echo "  (a restore over live data is a deliberate act: name the target)" >&2
  exit 2
fi
$PGBIN/psql -h "$SOCK" -p "$PORT" -U "$USER" -tAc "SELECT 1 FROM pg_database WHERE datname='$TARGET'" | grep -q 1 \
  || $PGBIN/psql -h "$SOCK" -p "$PORT" -U "$USER" -c "CREATE DATABASE \"$TARGET\"" >/dev/null
gunzip | $PGBIN/psql -h "$SOCK" -p "$PORT" -U "$USER" -q -d "$TARGET"
echo "restore.sh: restored into $TARGET"
