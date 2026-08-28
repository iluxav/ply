#!/bin/sh
# postgres — docker-library env contract, plus self-backup as composition:
# rclone is a dependency, so setting BACKUP_DEST makes the database dump
# itself on a schedule, and BACKUP_RESTORE makes a fresh volume restore
# itself before opening for traffic. Unset, both are inert.
#
#   POSTGRES_USER      superuser name            (default: postgres)
#   POSTGRES_PASSWORD  set → scram auth for TCP; local socket is trust
#   POSTGRES_DB        extra database created on first boot
#   PGPORT             listen port               (default: 5432)
#   POSTGRES_LISTEN    listen_addresses          (default: *)
#   BACKUP_DEST        rclone target (:s3:bucket/prefix, RCLONE_S3_* creds)
#   BACKUP_INTERVAL    seconds between dumps     (default: 86400)
#   BACKUP_KEEP_DAYS   prune uploads older than  (default: 14)
#   BACKUP_RESTORE     "latest" or a dump name — applied ONLY to an empty
#                      data dir, right after initdb, before serving
set -eu

PGBIN=/opt/postgresql17-17.10.0/usr/lib/postgresql/17/bin
PGDATA=/var/lib/postgresql/data
SOCK=/tmp
PGUSER="${POSTGRES_USER:-postgres}"
export RCLONE_CONFIG="${RCLONE_CONFIG:-/dev/null}"
# the keg closure ships no system CA store (ca-certificates builds its
# bundle in a maintainer script ply never runs) — rclone verifies TLS
# against the Mozilla bundle vendored with this app
export SSL_CERT_FILE="${SSL_CERT_FILE:-$PWD/cacert.pem}"
# bucket-scoped tokens may not HeadBucket/CreateBucket — skip the check
export RCLONE_S3_NO_CHECK_BUCKET="${RCLONE_S3_NO_CHECK_BUCKET:-true}"
[ -n "${POSTGRES_PASSWORD:-}" ] && export PGPASSWORD="$POSTGRES_PASSWORD"

fresh=""
if [ ! -s "$PGDATA/PG_VERSION" ]; then
  fresh=1
  echo "postgres: initializing $PGDATA"
  if [ -n "${POSTGRES_PASSWORD:-}" ]; then
    printf '%s\n' "$POSTGRES_PASSWORD" > /tmp/.pgpw
    # docker-library semantics: scram on the wire, trust on the local
    # socket — anyone who can exec into the container is already root
    # of this database's world, and backups/restores ride the socket.
    $PGBIN/initdb -D "$PGDATA" --auth-host=scram-sha-256 --auth-local=trust \
      --username="$PGUSER" --pwfile=/tmp/.pgpw --locale=C.UTF-8
    rm -f /tmp/.pgpw
    echo "host all all all scram-sha-256" >> "$PGDATA/pg_hba.conf"
  else
    echo "postgres: WARNING: no POSTGRES_PASSWORD — trust auth (dev only)"
    $PGBIN/initdb -D "$PGDATA" --auth=trust --username="$PGUSER" --locale=C.UTF-8
    echo "host all all all trust" >> "$PGDATA/pg_hba.conf"
  fi
fi

# First boot only: create POSTGRES_DB, then restore into it if asked.
# A failed restore wipes the data dir so the next start retries cleanly —
# a half-restored database that looks initialized is the one unforgivable
# outcome.
if [ -n "$fresh" ]; then
  $PGBIN/pg_ctl -D "$PGDATA" -w -o "-c listen_addresses='' -c unix_socket_directories=$SOCK" start
  if [ -n "${POSTGRES_DB:-}" ] && [ "$POSTGRES_DB" != "$PGUSER" ]; then
    $PGBIN/psql -h "$SOCK" -U "$PGUSER" -c "CREATE DATABASE \"$POSTGRES_DB\"" >/dev/null
  fi
  if [ -n "${BACKUP_RESTORE:-}" ]; then
    : "${BACKUP_DEST:?BACKUP_RESTORE needs BACKUP_DEST}"
    name="$BACKUP_RESTORE"
    [ "$name" = "latest" ] && name=$(rclone lsf "$BACKUP_DEST" | sort | tail -1)
    echo "postgres: restoring $BACKUP_DEST/$name into ${POSTGRES_DB:-$PGUSER}"
    if ! rclone cat "$BACKUP_DEST/$name" | gunzip | $PGBIN/psql -h "$SOCK" -U "$PGUSER" -q -d "${POSTGRES_DB:-$PGUSER}"; then
      $PGBIN/pg_ctl -D "$PGDATA" -m immediate stop || true
      rm -rf "$PGDATA"/*
      echo "postgres: restore FAILED — data dir wiped so the next start retries"
      exit 1
    fi
    echo "postgres: restore complete"
  fi
  $PGBIN/pg_ctl -D "$PGDATA" -m fast -w stop
fi

# Self-backup: pg_dump over the local socket, uploaded by the rclone
# layer, pruned by age. Runs beside the server; dies with the container.
if [ -n "${BACKUP_DEST:-}" ]; then
  (
    i=0; until $PGBIN/pg_isready -h "$SOCK" -q || [ $i -ge 60 ]; do i=$((i+1)); sleep 1; done
    while :; do
      name="${POSTGRES_DB:-$PGUSER}-$(date -u +%Y%m%d-%H%M%S).sql.gz"
      # dump to a file first: a known-size upload is one plain PUT, which
      # every S3 implementation accepts (R2 501s rclone's streaming mode)
      tmp="/tmp/.backup.sql.gz"
      if $PGBIN/pg_dump -h "$SOCK" -U "$PGUSER" "${POSTGRES_DB:-$PGUSER}" | gzip > "$tmp" \
         && rclone copyto "$tmp" "$BACKUP_DEST/$name"; then
        rclone delete --min-age "${BACKUP_KEEP_DAYS:-14}d" "$BACKUP_DEST" 2>/dev/null || true
        echo "postgres: backup ok ($name); next in ${BACKUP_INTERVAL:-86400}s"
      else
        echo "postgres: backup FAILED ($name); retrying in ${BACKUP_INTERVAL:-86400}s"
      fi
      rm -f "$tmp"
      sleep "${BACKUP_INTERVAL:-86400}"
    done
  ) &
fi

exec $PGBIN/postgres -D "$PGDATA" -p "${PGPORT:-5432}" -c listen_addresses="${POSTGRES_LISTEN:-*}" -c unix_socket_directories="$SOCK"
