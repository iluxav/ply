# umami stack

Self-hosted, privacy-first web analytics (a Google Analytics alternative)
as a ply stack: the umami app plus its Postgres database, wired in one
file.

## Run it locally

```sh
ply up
```

`ply.toml`'s `[stack]` starts both members dependency-ordered; umami waits
for the database, then serves on port 3000. Change the passwords and
`APP_SECRET` first.

## Deploy it on a host

Drop `deploy/umami-db.toml` and `deploy/umami.toml` into the deployments
dir (or paste this repo into the dashboard). They share `stack = "umami"`
so they group together; umami reaches the database at `umami-db.ply`.
Add a `domain = ["analytics.example.com"]` line to `umami.toml` to serve
it over TLS through the edge.

The database is the self-backing `postgres` keg — set `BACKUP_DEST` +
`RCLONE_S3_*` (ideally via a shared env file) for daily backups.

## What umami needs

- `DATABASE_URL` — `postgresql://postgres:<pw>@umami-db.ply:5432/umami`
- `DATABASE_TYPE=postgresql`
- `APP_SECRET` — any random string (session signing)

umami runs its own schema migrations on first start.
