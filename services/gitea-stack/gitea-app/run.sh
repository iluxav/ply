#!/bin/sh
# Generate a complete app.ini on first boot (with INSTALL_LOCK so gitea
# skips the web installer, the Postgres wiring, and gitea-generated
# secrets), then serve. Everything persists in the /data volume.
set -e
export GITEA_WORK_DIR=/data
export HOME=/data
CONF=/data/custom/conf/app.ini
mkdir -p /data/custom/conf /data/data /data/log
if [ ! -f "$CONF" ]; then
  SECRET=$(./gitea generate secret SECRET_KEY)
  TOKEN=$(./gitea generate secret INTERNAL_TOKEN)
  cat > "$CONF" <<INI
APP_NAME = ${GITEA_APP_NAME:-Gitea}
RUN_MODE = prod

[server]
HTTP_ADDR = 0.0.0.0
HTTP_PORT = 3000
DOMAIN = ${GITEA_DOMAIN:-localhost}
ROOT_URL = ${GITEA_ROOT_URL:-http://localhost:3000/}
SSH_DOMAIN = ${GITEA_DOMAIN:-localhost}

[database]
DB_TYPE = postgres
HOST = ${DB_HOST}
NAME = ${DB_NAME}
USER = ${DB_USER}
PASSWD = ${DB_PASSWD}

[security]
INSTALL_LOCK = true
SECRET_KEY = ${SECRET}
INTERNAL_TOKEN = ${TOKEN}

[service]
DISABLE_REGISTRATION = ${GITEA_DISABLE_REGISTRATION:-false}
INI
fi
# First boot: run migrations, then create an admin from env if asked. This
# runs as the git user (the entrypoint's own uid), so it can — unlike
# `ply exec`, which lands as root and gitea refuses.
if [ -n "${GITEA_ADMIN_USER}" ] && [ ! -f /data/.admin-created ]; then
  ./gitea migrate -c "$CONF"
  ./gitea admin user create -c "$CONF" \
    --username "${GITEA_ADMIN_USER}" \
    --password "${GITEA_ADMIN_PASSWORD}" \
    --email "${GITEA_ADMIN_EMAIL:-admin@localhost}" \
    --admin --must-change-password=false && touch /data/.admin-created
fi

exec ./gitea web
