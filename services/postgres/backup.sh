#!/bin/sh
# One dump, uploaded, old ones pruned — the unit of "backup". run.sh runs
# this every BACKUP_INTERVAL seconds beside the server; `ply backup now <db>`
# runs it through `ply exec`, which is why it carries its own PATH and
# library path instead of assuming the entrypoint's.
#
# Reports on stdout, one line, and (best effort) into /run/ply/self/backup
# so the host can read the last outcome without parsing logs:
#   backup ok <name> <when>      backup failed <name> <when>
set -u
PGROOT=$(echo /opt/postgresql17-*)
PGBIN="$PGROOT/usr/lib/postgresql/17/bin"
export LD_LIBRARY_PATH="$PGROOT/usr/lib/$(uname -m)-linux-gnu:$PGROOT/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
SOCK=/tmp
PORT=$(ls "$SOCK"/.s.PGSQL.* 2>/dev/null | grep -v '\.lock$' | head -1 | sed 's/.*\.//')
PORT="${PORT:-5432}"
export RCLONE_CONFIG="${RCLONE_CONFIG:-/dev/null}"
export RCLONE_S3_NO_CHECK_BUCKET="${RCLONE_S3_NO_CHECK_BUCKET:-true}"
: "${BACKUP_DEST:?BACKUP_DEST is not set — nowhere to put the dump (see the Backups guide)}"
USER="${POSTGRES_USER:-postgres}"
DB="${POSTGRES_DB:-$USER}"

name="$DB-$(date -u +%Y%m%d-%H%M%S).sql.gz"
when=$(date -u +%Y-%m-%dT%H:%M:%SZ)
# dump to a file first: a known-size upload is one plain PUT, which every
# S3 implementation accepts (R2 501s rclone's streaming mode)
tmp="/tmp/.backup.sql.gz"
if "$PGBIN/pg_dump" -h "$SOCK" -p "$PORT" -U "$USER" "$DB" | gzip > "$tmp" \
   && rclone copyto "$tmp" "$BACKUP_DEST/$name"; then
  rclone delete --min-age "${BACKUP_KEEP_DAYS:-14}d" "$BACKUP_DEST" 2>/dev/null || true
  size=$(wc -c < "$tmp" | tr -d ' ')
  rm -f "$tmp"
  echo "backup ok $name ($size bytes) $when"
  { printf 'ok %s %s\n' "$name" "$when" > /run/ply/self/backup; } 2>/dev/null || true
  exit 0
fi
rm -f "$tmp"
echo "backup failed $name $when" >&2
{ printf 'failed %s %s\n' "$name" "$when" > /run/ply/self/backup; } 2>/dev/null || true
exit 1
