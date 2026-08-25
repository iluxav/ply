---
title: Dependencies & lockfiles
description: How ply resolves version ranges with Minimal Version Selection and pins them in ply.lock.
section: Guides
order: 10
---

# Dependencies & lockfiles

## Declaring dependencies

```toml
[package]
# …
base   = "debian@13"         # exactly one base per app (owns /, libc, /bin/sh)

[dependencies]
node   = "22"                # any 22.x — range, not a pin
ffmpeg = { source = "github:someorg/ffmpeg-pkg", version = "6.1" }
```

The key is the package name; ranges are semver prefixes: `"22"` means
≥22.0.0 <23, `"6.1"` means ≥6.1.0 <6.2. The base lives under `[package]`
because it is a singular role, not a list entry — but it resolves, locks,
and fetches exactly like every other dependency. Its `name@range` string
form has a table twin for custom sources:
`base = { name = "debian", version = "13", source = "corp" }`.

Every dependency occupies its own prefix in the final filesystem —
`/opt/node-22.6.0/`, `/opt/ffmpeg-6.1.1/` — so two packages can never
conflict over a file. Their `PATH` and `LD_LIBRARY_PATH` contributions are
composed automatically in dependency order.

## Resolution: Minimal Version Selection

`ply build` resolves ranges the way Go modules do: pick the **lowest**
version that satisfies every constraint. No SAT solver, no "latest wins",
no surprise upgrades:

- The same manifest resolves to the same versions, forever.
- Resolution output changes **only when you edit the manifest** — an upgrade
  is a deliberate act, never a side effect of rebuilding.
- One version per package name per graph, enforced by the resolver
  (an overlay can't mount two ffmpegs at the same prefix — so the model
  doesn't pretend it can).

## The lockfile

`ply build` writes `ply.lock` next to your manifest:

- exact resolved version of every package
- the sha256 of every package file

The manifest is *human intent*; the lockfile is *machine truth*. Deploys use
the lockfile only — a production host never resolves anything, it fetches
exact hashes. Commit `ply.lock` to your repo like you would `package-lock.json`.

The hash is the trust boundary: sources are untrusted by design. A
compromised mirror can serve whatever it wants — wrong bytes fail the hash
check and the fetch is rejected.

## Upgrading

```sh
ply outdated        # list dependencies with newer versions available
```

To upgrade, widen or bump the range in `ply.toml` and rebuild. To patch a
runtime under an already-built app **without rebuilding it**:

```sh
ply rebase app.img --runtime node@22.6.1
```

Only the embedded lockfile changes — fleet-wide security patching becomes a
metadata operation plus one new store file.

## Vendoring vs declaring

For system-level needs there's a ladder — use the lowest rung that works:

1. **Vendor a static binary** in your app directory (what npm's
   `ffmpeg-static` already does — needs zero ply support)
2. **Declare a package** (shared in the store, cached, content-addressed)
3. **Fat mode** (`ply bundle`) — flatten everything for airgapped hosts
