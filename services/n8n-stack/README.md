# n8n stack

n8n (workflow automation, a Zapier/Make alternative) as a ply stack:
n8n + Postgres. n8n is the official image, imported once — smaller than a
from-source build, and upstream already pruned it.

## Get the image (one-time)
    ply import docker://n8nio/n8n:latest -o n8n.img

## Deploy on a host
Drop `deploy/n8n-db.toml` + `deploy/n8n.toml` into the deployments dir.
They share `stack = "n8n"`; n8n reaches the db at `n8n-db.ply`. The
`volumes = ["/data"]` line + `N8N_USER_FOLDER=/data` give n8n a writable,
chowned data dir (its image declares no VOLUME). Add
`domain = ["n8n.example.com"]` + `WEBHOOK_URL` to serve over TLS.

Needs ply >= (the release with --volume, --name, and zstd import).
