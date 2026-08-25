# Troubleshooting

Read the errno, not the message — ply surfaces the kernel's answer, and the
errno usually names the cause exactly.

## `mount rprivate at /: EACCES: Permission denied`

Rootless on a kernel that restricts unprivileged user namespaces (Ubuntu
24.04+). ply needs an AppArmor profile — the same requirement Docker and
Chrome have.

```sh
sudo ply setup
```

If you already ran it, the profile names a **specific binary path**: `which
ply` must match the path in `/etc/apparmor.d/ply`. Running a freshly built
binary from `target/` will fail this way even though the installed one works.

## `chown: …: Invalid argument` / `setuid: Invalid argument` (EINVAL)

Rootless, and the uid does not exist inside the user namespace. By default a
userns maps exactly one id — root inside is you outside — so anything
switching to a service uid fails. Breaks `[package] user` and every imported
image that runs `gosu`.

```sh
sudo apt install uidmap     # newuidmap / newgidmap
sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER
ply setup                   # reports whether both are now in place
```

Note the errno: **EINVAL means unmapped uid; EPERM means a missing
capability.** They look alike and have different fixes.

## `chown: …: Operation not permitted` (EPERM)

A capability was dropped. ply's default is zero capabilities. An imported
image should carry `capabilities = "oci"` — check the embedded manifest. For
your own package, this almost always means the app is doing work that
`[package] user` should do from the parent instead.

`ply run --privileged` confirms the diagnosis by skipping rights stripping.
It is for triage, never for running.

## `cannot bind 0.0.0.0:80: Address in use`

Something already holds the port. A backgrounded `ply run` parent survives
until killed, and a systemd unit may not have stopped cleanly:

```sh
ss -ltnp | grep -E ':80\s'          # names the owner
systemctl stop ply-<app>
pkill -TERM -f 'ply run'            # SIGTERM, so layers unmount cleanly
```

## `bind() to 0.0.0.0:80 failed (13: Permission denied)` from inside an app

Rootless cannot bind below 1024 — see `edge-tls.md`. Not a ply bug; rootless
Docker behaves identically.

## `expected PORT or HOST_PORT:INSTANCE_PORT`

The `ply` on that host predates the `[ADDR:]` grammar. Check with
`ply run --help | grep ADDR` and install a current build.

## An app cannot find its dependency

Check the dependency is actually published — `--after` injects nothing for an
unpublished app, by design, rather than inventing an address that fails
further away:

```sh
ply ps                              # is it running?
ss -ltnp | grep <port>              # did the parent bind?
```

Then confirm the app reads `<DEP>_ADDR` / `<DEP>_HOST` / `<DEP>_PORT` rather
than a hardcoded host.

## Rolling deploy hangs

A `[health]` gate is not passing. `ply ps` shows the stuck slot. The gate is a
TCP connect to `[health] port` within `grace` — a slow cold start needs a
bigger `grace`, not a removed gate.

## `ply proxy` refuses for a rootless app

Rootless instances share the host network and all report `127.0.0.1`, so
there is no per-instance address to emit. Publish the pool and the parent
becomes the single stable backend:

```sh
ply run app.img --publish internal:3000 --scale N
```

## The image is enormous

`include` is probably unset, so everything in the directory shipped —
`node_modules`, `.git`, build caches. Set it to just what runs. Check with
`ply check IMAGE`, and remember an imported Docker image is fat by nature
(a flattened snapshot, not a composition).
