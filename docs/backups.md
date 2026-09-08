---
title: Backups
description: A database that backs itself up on a schedule to any S3-compatible store, and comes back from it — tested, not hoped.
section: Guides
order: 16
---

# Backups

A service that holds state has to be able to come back. ply's answer is
the service's own: the registry's Postgres dumps itself on a schedule to
wherever you point it, keeps a window, and restores from there. ply's
verbs drive that, inside the instance, with the instance's own
environment, so the destination and its credentials are set once.

## Turning it on

Three variables on the service. In a stack file:

```toml
[[app]]
run = "postgres@17"
e = [
  "BACKUP_DEST=:s3:my-bucket/pg",     # an rclone target
  "BACKUP_INTERVAL=86400",            # seconds between dumps (default: a day)
  "BACKUP_KEEP_DAYS=14",              # prune older dumps (default: 14)
  "RCLONE_S3_PROVIDER=AWS",
  "RCLONE_S3_REGION=eu-central-1",
  "RCLONE_S3_ACCESS_KEY_ID=AKIA…",
  "RCLONE_S3_SECRET_ACCESS_KEY=enc:v1:…",   # sealed for this host — see Sealed secrets
]
egress = { allow = ["s3.eu-central-1.amazonaws.com"] }
```

`BACKUP_DEST` is any [rclone](https://rclone.org/) target: an S3 bucket
on AWS, Cloudflare R2, Backblaze, MinIO, DigitalOcean Spaces; or
`:local:/backups` for a directory you `--link` in from the host. The
credentials are rclone's environment variables; seal the secret one
([Sealed secrets](/docs/secrets/)) and the stack file can live in git.

The Postgres image declares `egress = []`, so a destination has to be
allowed by the operator, as above; under `enforce`, a backup to an
unlisted host fails and shows in `ply egress db --blocked`.

From the first boot the service dumps `POSTGRES_DB` with `pg_dump`,
gzipped, every `BACKUP_INTERVAL` seconds, named
`<db>-<UTC timestamp>.sql.gz`, and prunes anything older than
`BACKUP_KEEP_DAYS`. Each run prints one line to the service's log and
writes the last outcome to `/run/ply/self/backup`.

## Driving it

```sh
ply backup now db          # a dump outside the schedule; prints its name
ply backup ls db           # the dumps at BACKUP_DEST, oldest first
ply restore db --to check  # the latest dump, into a database named `check`,
                           # beside the live one — look at yesterday next to today
ply restore db app-20260908-030001.sql.gz --replace
                           # that dump, OVER the live database
```

`--replace` is the disaster path. Connections to the live database are
terminated, it is dropped and recreated, and the dump loads; the app
reconnects, as database clients do, and everything written since the dump
is gone. That is what a restore means, and the flag says so.

Both verbs run inside the instance through `ply exec`, so they work
wherever `ply exec` does, rootful or rootless, Linux or macOS, and need
nothing on the command line that the service does not already know.

## Coming back from nothing

The volume is gone, or the host is. Start the same service on an empty
volume with `BACKUP_RESTORE=latest` (or a dump's name) and the same
`BACKUP_DEST` and credentials: on first boot it fetches the dump and
loads it before the server takes connections. A failed restore wipes the
data directory so the next start retries cleanly, rather than leaving a
half-restored database that looks initialised.

```sh
ply run postgres@17 -e POSTGRES_DB=app -e BACKUP_DEST=:s3:my-bucket/pg \
  -e BACKUP_RESTORE=latest -e RCLONE_S3_… --publish internal:5432
```

`BACKUP_RESTORE` only ever applies to an empty volume; on a volume with
data it is ignored, so it is safe to leave in a stack file.

## Prove it before you need it

A backup nobody has restored is a hope. Once a month, or in CI against a
scratch bucket:

```sh
ply backup now db
ply restore db --to verify
ply exec db psql -U postgres -d verify -c 'select count(*) from your_table'
```

## Other services

The contract is small and any service can follow it: read `BACKUP_DEST`,
`BACKUP_INTERVAL`, `BACKUP_KEEP_DAYS` and `BACKUP_RESTORE`; ship
`backup.sh` (one dump, now) and `restore.sh` (a dump on stdin, `--to
NAME` or `--replace`) beside the entrypoint; depend on `rclone`. `ply
backup` and `ply restore` then work unchanged. Plain volumes with no
service to dump them are not covered here: snapshot the host's
filesystem, as the [Volumes](/docs/volumes/) guide says.
