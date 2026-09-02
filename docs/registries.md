---
title: Registries & publishing
description: Any file host is a ply registry — GitHub Releases, an R2 bucket, a directory. Zero API.
section: Guides
order: 11
---

# Registries & publishing

ply has no registry protocol. A source is a **URL template**; fetching is
*construct URL → HTTPS GET → verify sha256 → store*. That means any dumb
file host is a first-class registry, and there is no rate-limited API in
your deploy path.

## Source kinds

```toml
[sources]
default = "https://registry.plybox.sh/ply/{package}"   # the official registry
mine    = "github:iluxav/my-packages"                  # GitHub Releases
corp    = "https://artifacts.corp.net/ply"             # any https host
local   = "file:///srv/ply-packages"                   # a directory
```

| Spec | Resolves to |
|---|---|
| `github:org/repo` | `https://github.com/org/repo/releases/download/v{version}/{filename}` |
| `gitlab:group/proj` | `https://gitlab.com/group/proj/-/releases/v{version}/downloads/{filename}` |
| `https://host/path` | `https://host/path/{filename}` — `{package}` in the URL expands to the package name |
| `file:///path` | local directory |

Plain `http://` is allowed for localhost and RFC-1918 hosts (handy for a
LAN registry); public hosts require `--insecure-source` — the hash catches
tampering either way, this is hygiene.

Dependencies pick a source by alias:

```toml
[dependencies]
node    = "22"                                    # uses `default`
mytools = { source = "mine", version = "0.1" }    # uses the alias
```

## The official registry

`registry.plybox.sh` serves mainstream packages converted from Debian
trixie (glibc — npm prebuilts, pip wheels and JNI libraries work
untouched), plus runnable services under `apps/` — prebuilt,
content-addressed, **append-only** (a published version's manifest never
changes, and neither does an arch's bytes once uploaded — bump the
version instead, so lockfiles never rot). **Reading is a CDN**: no
accounts, no API, just HTTPS GET on the files below. **Publishing** goes
through `ply push` — see [CLI reference](/docs/cli/#registry-account).
Browse it at [registry.plybox.sh](https://registry.plybox.sh).

## The apps namespace

Everything official — kegs, bases, and the prebuilt services `ply run
postgres@17` fetches — lives under `ply/`, one namespace. Whether a
package is runnable is a property of its own manifest (it declares an
`entrypoint`), not which shelf it's filed under, so `ply/redis` is one
package now — a library other builds depend on and the service `ply run
redis@8` fetches — rather than two same-named packages kept apart to
avoid colliding. `apps/` is a **legacy alias**: it still serves the
copies pushed there before the namespaces unified, for anything still
pointed at it, but nothing new publishes there. `ply run --source` points
name resolution at any source spec that follows the layout, including
`file:///` directories.

## Publishing to the official registry

`ply push` is two HTTP calls: upload the bytes (mechanical), then publish
the record (the manifest — that call is the actual publish). See
[`ply push`](/docs/cli/#registry-account) for the full command; here is
the shape it sends and what the registry does with it.

**The record** — one JSON document per (owner, name, version), the same
shape for apps, kegs, and stacks:

```json
{
  "owner": "ply", "name": "postgres", "version": "17.10.7", "type": "app",
  "manifest_toml": "<ply.toml, verbatim, comments included>",
  "manifest": { "package": { "name": "postgres", "...": "..." }, "...": "..." },
  "artifacts": [
    { "arch": "x64", "src": "https://registry.plybox.sh/ply/postgres/postgres-17.10.7-linux-x64.img",
      "sha256": "0cf85787…", "bytes": 110592, "verified": true }
  ]
}
```

`manifest` is `manifest_toml` rendered to JSON, key for key — nothing
derived, nothing filled in; `manifest_toml` is the document people read,
`manifest` is for queries and consumers without a TOML parser. A stack's
record has `artifacts: []` and `manifest_toml` = the stack file verbatim,
`$VAR` holes intact.

Once a version's `manifest_toml` is published it is fixed: republishing
the same version with a different manifest is refused (append-only, at
artifact granularity); a later push may still add an artifact for an arch
that isn't there yet.

**`verified`** is per artifact: `true` only when the registry stored the
bytes itself and hashed them; `false` when you supplied `--src` and the
CLI computed the hash locally instead. The registry does not fetch
external artifacts to check them in v1 — a later verification pass may
flip the flag without a protocol change.

**`--src`** publishes an artifact that lives elsewhere instead of
uploading it — a release asset, any static host:

```sh
ply push myapp-1.2.0-linux-x64.img \
  --src https://github.com/you/app/releases/download/v1.2.0/myapp-1.2.0-linux-x64.img
```

`{version}` and `{arch}` expand in the URL template, so one `--src` covers
every arch and version you push. The bytes still have to exist locally —
ply hashes them itself rather than asking the registry to fetch a URL —
and the artifact records `verified: false`.

## Publishing your own packages

Because a registry is just files, publishing is copying:

**GitHub Releases** (the common case) — name the release tag `v<version>`
and upload the image as an asset. `ply build` already names artifacts
canonically, so a CI job is two lines:

```yaml
- run: ply build .
- run: gh release upload v1.2.0 myapp-1.2.0-linux-x64.img
```

**Any web host / bucket** — upload the image under your chosen prefix.

**A directory** (great for testing and airgapped deploys):

```sh
mkdir -p /srv/ply-packages
cp myapp-1.2.0-linux-x64.img /srv/ply-packages/
# … and on the consuming side:
# [sources] default = "file:///srv/ply-packages"
```

## Version listing (index.json)

Fetching a *pinned* version needs nothing but the filename. Resolving a
*range* (`node = "22"`) needs to know which versions exist. On plain http(s)
hosts, publish an `index.json` next to the images — a JSON array of
filenames:

```json
["node-22.5.0-linux-x64.img", "node-22.6.0-linux-x64.img"]
```

Directory sources list files directly, no index needed. Forge sources
(GitHub/GitLab) can't list versions yet — pin exact versions for them.
The official registry's `index.json` also lists each version's
`<name>-<version>.toml` (its manifest) alongside the images — ply's own
version listing ignores anything that isn't an image filename, so a
self-hosted `index.json` needs only the array above.

## Catalog (state.json)

`ply search` and `ply add` read an optional catalog at the source's
**prefix** — the template with `/{package}` removed:

| source | catalog |
|---|---|
| `https://registry.plybox.sh/ply/{package}` | `https://registry.plybox.sh/ply/state.json` |
| `https://artifacts.corp.net/ply` | `https://artifacts.corp.net/ply/state.json` |
| `file:///srv/ply-packages/{package}` | `/srv/ply-packages/state.json` |

The official registry publishes it; for your own host, a minimal one is:

```json
{ "packages": [
  { "name": "ffmpeg", "description": "Multimedia framework", "license": "LGPL-2.1",
    "versions": [
      { "version": "6.1.1", "img": "ffmpeg-6.1.1-linux-x64.img" },
      { "version": "6.1.1", "img": "ffmpeg-6.1.1-linux-arm64.img" } ] } ] }
```

Only `name`, `version` and `img` are required; `arch` is derived from the
filename when absent. Forge sources have no catalog. Neither `index.json`
nor `state.json` is needed to fetch a pinned version.

The official registry's `state.json` carries more per version, all of it
additive — a minimal catalog with just `name`/`version`/`img` still
works, and an older ply reading a v3 catalog ignores fields it doesn't
know: `manifest` (the version's `.toml` URL) and `verified`, plus fields
derived from the manifest for consumers without a parser — `volumes`,
`links`, `publish`, `dependencies`, `params` — where the `.toml` stays the
source of truth. A stack entry has `img: null`; both `src` and `manifest`
point at its `.toml`.

Additive means new **keys**, never a changed **type**. A catalog is parsed
in one pass into typed structs, so a field whose type changes doesn't
degrade — it fails the whole document, and with it every `ply search`,
`ply add` and `ply up <ns>/<stack>` on every ply already installed. That
is why `dependencies` stays the array it has always been
(`[{"name": "postgresql17", "version": "17"}]`) even though the registry
holds it as a map internally.

## Private packages

For a private artifact host, anything your network can GET works (VPN,
signed URLs, LAN). The hash check makes the transport untrusted by design.
Token-authenticated GitHub sources are on the roadmap.

## The testing gift

```sh
python3 -m http.server 8000 --directory /srv/ply-packages
```

…is a complete, working registry. ply's own integration tests spin one up
in a tempdir and run the full resolve→fetch→verify path offline in
milliseconds. Airgapped deploys are the same trick: rsync the directory,
use a `file://` source.
