# Registry: the manifest is the record

**Date:** 2026-09-02 · **Status:** approved in discussion, spec for review · **Repos:** `ply` (CLI + core), `ply-web` (registry API + site, nested at `app/`)

## The problem being deleted

Today `ply push` derives a typed summary of an image (`type`, `volumes`, `links`, `dependencies`, `apps`) on the client, ships it in an `X-Ply-Meta` header, and the registry stores each field as its own jsonb column, re-emits them into `state.json`, and the site renders from those columns. A second, hand-authored truth (`registry/meta/<owner>/<name>.json`: description, license, homepage, `publish`, a free-text env `contract`) is merged in by a script. Adding `[params]` to manifests showed the cost: one new manifest table needs a CLI change, a header change, a route change, a DB column, a state.json field, and a page change. The derived columns are a cache pretending to be the model.

The vision the registry must serve: ply is Docker rethought, daemonless, "everything is a file". Anyone who does not want plybox.sh runs their own registry by changing `[sources]` in a toml file. A schema of our own makes that hard to replicate and is not needed: `ply.toml` is already a complete structure, like `package.json` or `Cargo.toml`, and the CLI converts it to JSON in milliseconds.

## The record

One document per (owner, name, version); the same shape for apps, kegs (layers), and stacks.

```json
{
  "owner": "ply", "name": "postgres", "version": "17.10.7", "type": "app",
  "manifest_toml": "<the file, verbatim, comments included>",
  "manifest": { "package": {"name": "postgres", "version": "17.10.7", "...": "..."},
                "dependencies": {"postgresql17": "17", "rclone": "1.60"},
                "params": {"user": "postgres", "password": {"secret": true}, "...": "..."},
                "env": {"POSTGRES_USER": "{user}", "...": "..."} },
  "artifacts": [
    {"arch": "x64",   "src": "https://registry.plybox.sh/ply/postgres/postgres-17.10.7-linux-x64.img",
     "sha256": "0cf85787…", "bytes": 110592, "verified": true},
    {"arch": "arm64", "src": "https://myserver.com/pg-17.10.7-arm64.img",
     "sha256": "803b5552…", "bytes": 110592, "verified": false}
  ],
  "pushed_at": "2026-09-02T15:51:37Z",
  "published_by": "iluxav"
}
```

Rules:

- `manifest` is the CLI's JSON rendering of `manifest_toml`, key for key (`toml::Value` → JSON, no derivation, no defaults filled in). `manifest_toml` is the document people read; the JSON is for queries and for consumers without a TOML parser.
- `type` is derived once at publish: `[stack]` present → `stack`; `[package] entrypoint` present → `app`; otherwise `layer`. It exists so listings filter without parsing.
- `owner` comes from `[package] owner` (new optional field; for stacks `[stack] owner`) when present, else from the token's login. Either way the token must hold a grant for that namespace (`namespace_grants`, unchanged). A file claiming an owner the token cannot publish to is refused.
- `[package]` gains optional `owner`, `description`, `license`, `homepage`. The hand-authored `registry/meta/*.json` files retire: their description/license/homepage move into the kegs' manifests; `contract` is superseded by `[params]`; `publish` is derived from `[ports]` (see "Derived fields").
- A stack's record has `artifacts: []` and `manifest_toml` = `stack.toml` verbatim, `$VAR` holes intact. `[stack] name` and `version` name it.
- Immutable per version: the first publish fixes `manifest_toml`. A later publish of the same version must carry a byte-identical `manifest_toml` and may only add artifacts for arches not yet present. An existing artifact is never replaced. (Today's append-only rule, at artifact granularity.)
- `verified` is per artifact: `true` when plybox.sh stored the bytes and hashed them itself; `false` when the publisher supplied `src` and the sha256 was computed by the CLI. The registry is not a verifier in v1; a later background verifier may flip the flag or quarantine a mismatch without a protocol change.

## The read side is files

Every consumer reads only these. A self-hosted registry is any host that serves them; `[sources] default = "https://john-server.com/{package}"` resolves against exactly this layout.

```
{owner}/{name}/index.json                            all versions with their artifacts
{owner}/{name}/{name}-{version}.toml                  manifest_toml
{owner}/{name}/{name}-{version}-linux-{arch}.img      when plybox.sh hosts the bytes
{owner}/state.json                                    every record in the namespace, for search and `ply up owner/stack`
```

### `state.json` v3 (additive over v2; old CLIs keep working)

Per package (unchanged keys): `namespace`, `name`, `type`, `description`, `license`, `homepage` (the last three now read from `manifest.package`), `versions[]`.

Per version entry (one per artifact, as today, so `arch` stays a flat field): `version`, `arch`, `src`, `img`, `bytes`, `sha256`, `pushed_at` (unchanged) plus:

- `manifest`: URL of the `.toml` for this version (same URL for every arch of a version).
- `verified`: boolean.
- `params`, `volumes`, `links`, `publish`, `dependencies`: **derived fields**, computed by the server from `manifest` when it writes `state.json` (see below). They are a cache for consumers without a parser (the Go dashboard, the site's listings); the `.toml` is the truth.

A stack entry has `img: null`, `src` = its `.toml` URL, `manifest` = the same URL (as today's `.stack.toml` artifact, renamed). `apps` is no longer emitted: `ply up owner/stack` parses the `.toml`.

### Derived fields (server-side, from `manifest`)

| field | derivation |
|---|---|
| `type` | `[stack]` → stack; `package.entrypoint` → app; else layer |
| `description`, `license`, `homepage` | `package.*` |
| `volumes` | `volumes.*.path` |
| `links` | `requests.links` |
| `publish` | `ports`: one port → `internal:<port>`; else omitted (replaces the hand-authored hint) |
| `dependencies` | `dependencies` as written (name → range). Note: today's field carried the image's *pinned* lock; the page shows constraints from now on. The lock stays inside the image. |
| `params` | `params` as written, secrets carry no value (`{secret = true}` stays a table) |

The derivation is ~40 lines of TypeScript over `manifest`, with a `--rebuild` path that regenerates every `index.json` and `state.json` from the table (the existing `--state-only` shape).

## The push protocol

Two endpoints. Bytes are mechanical; the record is the publish.

```
POST /api/upload/                 body: image bytes (octet-stream)
  headers: Authorization, X-Ply-Filename, X-Ply-Sha256 (CLI-computed)
  → 200 {"src": "https://registry.plybox.sh/{owner}/{name}/{filename}", "sha256": "…", "bytes": n, "verified": true}
  409 if the filename exists with different bytes; 200 (idempotent) if identical.

POST /api/publish/                body: the record minus pushed_at/published_by (JSON)
  → 201 {record}            first publish of the version, or an added arch
  → 200 {record}            identical artifact re-published (idempotent)
  → 409 {"error": "…", "diff": "…"}  manifest differs from the one on record / artifact exists with other bytes
  → 403                     token cannot publish under `owner`
```

Server rules on publish: the token must be granted for `owner`; `manifest` must equal the CLI-independent JSON rendering of `manifest_toml` (the server re-parses the TOML to check; a mismatch is a 400 — the one place the server parses TOML, so a hand-written record cannot lie about its manifest); an artifact `src` under `registry.plybox.sh/{owner}/{name}/` must name an object the upload endpoint issued (else 400); any other host is recorded as external with `verified: false`. After a publish the server rewrites `{owner}/{name}/index.json`, `{owner}/{name}/{name}-{version}.toml`, and `{owner}/state.json` (reads stay static).

What the CLI does:

| command | build | upload | publish |
|---|---|---|---|
| `ply push .` (app/keg dir) | yes | yes | manifest read from the built image's `/.manifest.toml`, artifact `verified: true` |
| `ply push image.img` | no | yes | same |
| `ply push . --src URL` | yes (for sha256/bytes) | no | `verified: false`; `URL` may be a template with `{version}` and `{arch}` |
| `ply push image.img --src URL` | no | no | same |
| `ply push .` (stack dir) / `ply push stack.toml` | no | no | `type: stack`, `artifacts: []`; members must be registry refs or URLs (a `./dir` member is refused, as today) |
| `ply push … --arch arm64` | cross-build, as `ply build --arch` | yes/no | appends the arm64 artifact to the version |
| `ply push … --dry-run` | as above | no | prints the record instead of sending it |
| `ply push … --as NAMESPACE` | | | sets `owner` when the manifest has none; conflicts with a different `[package] owner` |

The manifest published for an image is the one embedded in the image (what the artifact actually contains), never the working-copy `ply.toml`. `X-Ply-Meta` and `derive_push_meta`/`derive_stack_meta` are deleted from the CLI.

## Server storage

```sql
CREATE TABLE records (
  id            serial PRIMARY KEY,
  package_id    int NOT NULL REFERENCES packages(id),      -- ownership + grants stay on `packages`
  version       text NOT NULL,
  type          text NOT NULL,                              -- app | layer | stack, derived
  manifest_toml text NOT NULL,
  manifest      jsonb NOT NULL,
  artifacts     jsonb NOT NULL DEFAULT '[]'::jsonb,         -- [{arch, src, sha256, bytes, verified}]
  pushed_at     timestamptz NOT NULL DEFAULT now(),
  published_by  int REFERENCES users(id),
  UNIQUE (package_id, version)
);
CREATE INDEX records_manifest_gin ON records USING gin (manifest);
```

`packages` keeps `owner`, `name`, `type` (mirror of the latest record, for listings). `versions` and its derived columns are dropped after the backfill. `events` unchanged. All catalog reads on the site go through `state.json` as they do today, so the site's data path does not change shape, only source.

## Migration

1. **Registry first.** Deploy the new table, both endpoints, and the derived-fields writer. The old push route keeps accepting `X-Ply-Meta` pushes for one CLI release: the server cannot read squashfs, so such a record is stored with `manifest_toml: ""` and only the legacy derived columns, the page shows "manifest unavailable, re-push with ply ≥ 0.1.70", and the backfill (step 2) fills it in. Old CLIs reading `state.json` see only additive fields.
2. **Backfill.** A one-off script run from the owner's box: for every version in the live `state.json`, fetch the image, read `/.manifest.toml` with the CLI (`ply push image.img --dry-run --src <its current src>` prints the record), and POST it to `/api/publish/` with `--backfill`, a flag the endpoint accepts only while the version has no record; it sets `verified: true` for plybox.sh-hosted artifacts whose sha256 matches the stored row. Stacks backfill from their `.stack.toml` objects. The deb/apk keg pipeline (`scripts/registry-push.mjs`, which writes R2 and `state.json` directly and never used the API) gains the same step: upload the `.toml` beside each image and emit `manifest`/derived fields in the state it writes.
3. **CLI release** (0.1.70): new push, `ply inspect REF`, catalog reader on v3 fields, `fetch_stack` over the `.toml`, `[package] owner/description/license/homepage`.
4. **Retire** `registry/meta/*.json` and its merge step after the kegs carry the fields; drop `versions`.

## CLI surface beyond push

- `ply inspect REF|IMAGE|DIR`: resolves a registry ref like `ply run` (fetching into the store if needed), reads the manifest, and prints: type, volumes, links, dependencies, **params** (each declared param with its kind: default / computed / secret, minted / secret, external), and the built-in facts and live names as a fixed footer. `--json` prints the record (same as `push --dry-run` for an image).
- `ply up owner/stack` and `ply search|add|init`: read v3; nothing user-visible changes except that a stack no longer needs `apps` in the state.

## Testing

- CLI: record rendering from a TOML-literal manifest (all param kinds; stack; layer), `--src` template expansion, `--dry-run` byte-exact record, refusal of `./dir` stack members, idempotent re-push of the same artifact, arch append; `fetch_stack` over a local `[sources]` directory laid out per "The read side is files" (this doubles as the self-hosted-registry test).
- Registry: publish rules (grant, manifest match on second arch, artifact immutability, external src unverified), derived fields from a fixture manifest, state.json v3 emission, backfill flag lifecycle.
- End to end: push the two kegs and the todos stack from a scratch namespace, `ply up <ns>/todos --plan` from a clean box.

## Out of scope (deliberately)

- A verification service (hash re-check of external artifacts). The flag is in place for it.
- The static `ply push --out DIR` writer and multi-source search (federation). The layout is defined so both are additive later.
- Any change to image format, lockfiles, or resolution; the lock stays inside the image.
- Registry UI beyond rendering the manifest on the package page (params, ports, volumes, dependencies, env contract) and a `verified` mark per artifact.
