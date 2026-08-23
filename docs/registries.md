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

`registry.plybox.sh` serves mainstream Alpine packages converted to ply
images — prebuilt, content-addressed, **append-only** (a published version
never changes or disappears, so lockfiles never rot). It's a read-only CDN:
no accounts, no uploads, no API. Browse it at
[registry.plybox.sh](https://registry.plybox.sh).

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
