---
title: Image format & store
description: The filename grammar, deterministic squashfs format, content-addressed store, and registry file protocol.
section: Reference
order: 22
---

# Image format & store

## Filename grammar

```
<name>-<semver>-<os>-<arch>.img
ffmpeg-6.1.0-linux-x64.img
```

Strict and tool-enforced: `ply build` emits it automatically; names may not
contain `-` followed by a digit (parsing ambiguity). The filename is an
**identity claim**; the lockfile's sha256 is the **proof**. Anyone can name
a file `node-22.6.0-linux-x64.img`; only the right bytes will hash-verify.

Supported platforms: `linux-x64`, `linux-arm64`.

## The image file

A ply image is a **deterministic squashfs**: entries sorted, timestamps
zeroed, ownership fixed, zstd-compressed. The same input directory produces
a byte-identical file, every time — content-addressing depends on it, and
you can prove a build honest by rebuilding it.

Inside, at a known path, lives the embedded manifest (and for app images
the lockfile) — an image is self-describing. Images mount directly
(no extraction); on hosts that can't loop-mount, ply extracts to a plain
store directory with the same hash identity.

## The store

```
/var/lib/ply/
├── store/
│   └── sha256:<hash>/     # immutable; the dir name proves the content
└── volumes/
    └── <app>/<name>.<n>/  # named volumes, per instance slot

/run/ply/                  # tmpfs — gone on reboot
├── state/<app>.<n>.json   # pid, ip, ports per instance
└── instances/<app>.<n>/   # overlay upper/work/merged dirs
```

- Same hash = one copy: dedup by construction, across every app on the host
- `ply gc` = reachability from installed/running apps' lockfiles; anything
  unreferenced is deleted
- `rm -rf /var/lib/ply` is a complete factory reset — ply never touches
  `/bin`, `/usr`, or `/etc` beyond its managed hosts entries
- Rootless mode keeps the same layout under your home directory

## How images compose at run time

Dependencies are separate store entries. At run, ply loop-mounts each
squashfs read-only and stacks them with overlayfs — app on top, base at the
bottom, order derived deterministically from the dependency graph. Each
package owns `/opt/<name>-<version>/`, so layers can't conflict. A writable
tmpfs upper layer catches scratch writes; volumes bind-mount over their
declared paths.

You declare a *set*; ply derives the *stack* — the inverse of Docker, where
the stack is authored and baked at build time.

## Registry file protocol

A registry is a file host. The full protocol:

```
<base-url>/<name>-<version>-<os>-<arch>.img    # GET an image
<base-url>/index.json                          # optional: list for range resolution
<prefix>/state.json                            # optional: catalog for ply search / ply add
```

`index.json` is a JSON array of image filenames present at that prefix:

```json
["node-22.5.0-linux-x64.img", "node-22.6.0-linux-x64.img"]
```

The official registry publishes `state.json` at the bucket root (for
[plybox.sh/registry](https://plybox.sh/registry/)) and at each namespace
prefix (for `ply search`); see [Registries](/docs/registries/). Neither file
is needed to fetch pinned versions; fetching is always just a GET plus a
hash check.
