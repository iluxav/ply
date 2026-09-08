---
title: Backups
description: Snapshot any app's volumes as a dated image, restore by rolling it back in — nothing in the image has to cooperate.
section: Guides
order: 16
---

# Backups

An app's state lives in its `[volumes]`: the rootfs is read-only and the
overlay's scratch is thrown away on purpose. So a backup is a copy of the
volumes, and ply takes it the way `ply craft commit` takes an overlay:
commit the directory as an image.

```sh
ply snapshot take db            # every declared volume of db, as one dated image
ply snapshot ls db              # what you have, oldest first
ply restore db                  # the latest, back into the slot it came from
ply restore db db-snapshot-20260908.182535.1
```

Nothing in the image has to cooperate. A Postgres from the registry, a
`docker://` import, your own app with an uploads folder or a SQLite file:
a `[volumes]` entry is the whole contract.

## How a snapshot is taken

The one difference from a craft commit is that a craft session has left
its shell when it commits, and a running database has not; it is always in
the middle of writing. A file copy taken under it is not a backup, it is a
photograph of a page while someone is writing on it. So, inside the
instance, as the app's own user, ply holds every process of the app still
for the seconds the copy takes, streams the volumes out as a tar, and lets
them go. What comes out is exactly what a power cut would leave, which
every real database is built to recover from by replaying its log. A
killed `ply snapshot` cannot leave the app frozen: the thaw is on a trap.

The pause is the length of the copy: a second or two for the sizes a
single server holds, during which requests wait in the socket queue and
are answered after. For a large database, the service's own dump (below)
is smaller and needs no pause.

The copy is streamed through `ply exec`, so it works identically rootful,
rootless and inside a macOS microVM, needs no host path, and carries the
ownership the restore has to reproduce.

## What a snapshot is

An ordinary ply image, in the store under `snapshots/<app>/`, named
`<app>-snapshot-<YYYYMMDD>.<HHMMSS>.<slot>`: hashed, dated, listable, and
`ply craft edit` opens a shell on one to look at yesterday's files. Inside,
`/volumes/<name>/…` per declared volume, keyed by name so a manifest that
later moves a volume still restores it. `ply run` refuses it, correctly:
it has no entrypoint.

## How a restore works

A restore is a roll, the same path a deploy takes. The slot the snapshot
came from is stopped; its volume directories are moved aside, kept, never
deleted; fresh ones are filled from the image before the app starts, by
the instance's own init, inside its user namespace, so the files come out
owned by the app's ids rootless as well as rootful; then the app starts
and passes its health gate. For a single database that is the short outage
a restore inherently is. A scaled app restores slot by slot.

The previous volume stays under the app's volumes directory, in
`.pre-restore/`, until you remove it: a restore that turned out to be the
wrong one has destroyed nothing.

## Off the machine

Snapshots are images, so they go where images go: copy the file, `scp` it,
push it to a private registry with `ply push`. A snapshot on the same disk
survives a bad deploy, not a dead disk; keep the ones that matter
elsewhere. Scheduled snapshots and an S3 destination are the next step.

## A service that dumps itself

Some services know a better backup than a file copy: `pg_dump` is far
smaller than a Postgres data directory and needs no pause. The registry's
Postgres ships that, driven the same way:

```sh
ply backup now db                    # a dump to BACKUP_DEST, outside the schedule
ply backup ls db
ply backup restore db --to check     # beside the live data
ply backup restore db --replace      # over it: connections terminated, data since the dump gone
```

Set `BACKUP_DEST` (an rclone target: S3, R2, MinIO, `:local:/backups`),
`BACKUP_INTERVAL`, `BACKUP_KEEP_DAYS`, and rclone's `RCLONE_S3_*`
credentials on the service, sealed ([Sealed secrets](/docs/secrets/)); the
image declares `egress = []`, so allow the destination in the stack file.
`BACKUP_RESTORE=latest` on an empty volume restores on first boot, which is
the path back from a dead disk. Any service can follow the contract: read
those variables, ship `backup.sh` and `restore.sh` beside the entrypoint,
depend on `rclone`. Nobody has to; the snapshot above works regardless.

## Prove it before you need it

A backup nobody has restored is a hope. Take a snapshot, write something,
restore, look:

```sh
ply snapshot take db
ply exec db psql -U postgres -d app -c 'insert into t values (1)'
ply restore db
ply exec db psql -U postgres -d app -c 'select count(*) from t'   # the row is gone
```
