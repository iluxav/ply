#!/bin/sh
# pg-backup — pg_dump on an interval, uploaded with rclone, pruned by age.
# The whole app is this script: pg_dump comes from the postgresql-client17
# layer, rclone from the rclone layer. Config is env only.
#
#   POSTGRES_HOST/PORT/USER/DB/PASSWORD   the database (POSTGRES_ADDR works too)
#   BACKUP_DEST                           rclone target, e.g. :s3:my-bucket/pg
#   RCLONE_S3_*                           credentials for an :s3: dest
#   BACKUP_INTERVAL                       seconds between dumps (default 86400)
#   BACKUP_KEEP_DAYS                      prune uploads older than this (default 14)
#
# A failed dump exits nonzero: the restart policy respawns it, the crash is
# an instance-restart event, and notify can escalate it.
set -eu

if [ -n "${POSTGRES_ADDR:-}" ]; then
  : "${POSTGRES_HOST:=${POSTGRES_ADDR%%:*}}"
  : "${POSTGRES_PORT:=${POSTGRES_ADDR##*:}}"
fi
: "${POSTGRES_HOST:?where is the database? set POSTGRES_HOST (or POSTGRES_ADDR)}"
: "${POSTGRES_PORT:=5432}"
: "${POSTGRES_USER:=postgres}"
: "${POSTGRES_DB:=postgres}"
: "${POSTGRES_PASSWORD:?}"
: "${BACKUP_DEST:?rclone destination, e.g. :s3:my-bucket/pg (RCLONE_S3_* for creds)}"
: "${BACKUP_INTERVAL:=86400}"
: "${BACKUP_KEEP_DAYS:=14}"
export PGPASSWORD="$POSTGRES_PASSWORD"
# no config file — everything arrives via RCLONE_* env; this silences the
# "config not found" notice on every run
export RCLONE_CONFIG="${RCLONE_CONFIG:-/dev/null}"
export SSL_CERT_FILE="${SSL_CERT_FILE:-$PWD/cacert.pem}"
export RCLONE_S3_NO_CHECK_BUCKET="${RCLONE_S3_NO_CHECK_BUCKET:-true}"

if [ "${BACKUP_CHECK:-}" = "1" ]; then
  pg_dump --version && rclone version --check=false | head -1
  exit 0
fi

while :; do
  name="$POSTGRES_DB-$(date -u +%Y%m%d-%H%M%S).sql.gz"
  echo "pg-backup: $POSTGRES_DB@$POSTGRES_HOST:$POSTGRES_PORT -> $BACKUP_DEST/$name"
  pg_dump -h "$POSTGRES_HOST" -p "$POSTGRES_PORT" -U "$POSTGRES_USER" "$POSTGRES_DB" \
    | gzip > /tmp/.backup.sql.gz
  rclone copyto /tmp/.backup.sql.gz "$BACKUP_DEST/$name" && rm -f /tmp/.backup.sql.gz
  rclone delete --min-age "${BACKUP_KEEP_DAYS}d" "$BACKUP_DEST" 2>/dev/null || true
  echo "pg-backup: ok ($name); next in ${BACKUP_INTERVAL}s"
  sleep "$BACKUP_INTERVAL"
done
