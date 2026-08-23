# `ply search`, `ply add`, `ply init` — design

Date: 2026-08-22. Status: approved 2026-08-22; implementation in progress.

## Problem

A developer who wants a package today has to open plybox.sh/registry and
search by hand to learn whether it exists, which versions are published, on
which architectures, and what line to put in `ply.toml`. Cargo answers all of
that from the terminal (`cargo search`, `cargo add`). ply should too — without
inventing a registry protocol: a source stays a URL template, and the catalog
stays a static file.

## Scope

In: `ply search`, `ply add`, `ply init`, the catalog file contract, the
publisher change, docs. Out (deliberately): `ply info`, `ply remove`, searching several sources
at once, arch filtering, local caching, a built-in default source in the
resolver.

## 1. Catalog contract

A source MAY publish `state.json` at its **prefix**. The prefix is the source
template with `/{package}` and everything after it removed; a template without
the placeholder is its own prefix.

| source | catalog |
|---|---|
| `https://registry.plybox.sh/ply/{package}` | `https://registry.plybox.sh/ply/state.json` |
| `https://artifacts.corp.net/ply` | `https://artifacts.corp.net/ply/state.json` |
| `file:///srv/ply-packages/{package}` | `/srv/ply-packages/state.json` |
| `github:org/repo`, `gitlab:group/proj` | none — "searching a forge source is not supported; pin a version" |

Schema: the existing registry `state.json`, unchanged.

```json
{ "updated": "…", "package_count": 1, "image_count": 2, "total_bytes": 0,
  "packages": [ { "namespace": "ply", "name": "ffmpeg",
                  "description": "…", "license": "…", "homepage": "…",
                  "versions": [ { "version": "6.1.1", "img": "ffmpeg-6.1.1-linux-x64.img",
                                  "arch": "x64", "path": "ply/ffmpeg/…", "bytes": 0,
                                  "pushed_at": "…" } ] } ] }
```

The parser requires only `packages[].name` and `packages[].versions[].version`
+ `img`; every other field defaults (empty string / 0 / derived). `arch`
missing → derived from the `img` filename, as the website does. Third-party
sources can therefore publish a minimal catalog by hand.

`index.json` remains the contract for range resolution; `state.json` is only
read by `search` and `add`. Neither is needed to fetch pinned versions.

## 2. `ply search`

```
ply search <query> [--versions] [--limit N] [--source SPEC] [--json]
```

**Source resolution** (shared with `add`): `--source SPEC` (any spec
`Source::parse` accepts) → else `[sources] default` of `./ply.toml` if the
file exists → else `ply_core::OFFICIAL_SOURCE`
(`https://registry.plybox.sh/ply/{package}`), a new constant.

**Matching**: case-insensitive substring on `name` and `description`.
**Ranking**: exact name, name prefix, name substring, description match; ties
alphabetical by name. **Limit**: default 20, `0` = unlimited; when truncated,
print `… and N more — narrow the query or pass --limit 0` on stderr.

**Default row** — paste-ready dependency line, aligned:

```
ffmpeg = "6.1"        # Multimedia framework for audio/video…   x64 arm64
ffmpeg-libs = "6.1"   # FFmpeg shared libraries                  x64 arm64
```

- `"6.1"` is the latest version's `major.minor` — a ply range, the same rule
  as the website's `depLine`. Latest = max semver across all arches.
- arches = those of the latest version, sorted `x64` before `arm64`.
- description truncated to 60 chars with `…`; empty → `(no description)`.
- non-`ply` namespace → `name = { source = "<ns>", version = "6.1" }`.

**`--versions`** — one block per match:

```
ffmpeg   Multimedia framework for audio/video   (LGPL-2.1-or-later)
  6.1.1   x64 arm64   8.3 MiB
  6.0.1   x64         8.1 MiB
```
Versions descending; size is the largest image of that version; `—` when 0.

**`--json`**: the ranked matches as an array of the catalog package records,
with an added `"latest": "6.1.1"` field. Empty array when nothing matches.

**Exit codes**: 0 on success including no matches (`no packages match "x"`
to stderr); 1 on a missing/invalid catalog or network error.

## 3. `ply add`

```
ply add <name>[@<range>] [--source NAME]
```

Edits `./ply.toml` (cwd only in v1; run it where `ply build .` runs).

1. Parse the manifest with `toml_edit` (format-preserving). Missing file →
   error `no ply.toml in <cwd>`.
2. Decide the source: `--source NAME` must be a key of `[sources]`; else
   `default`. If `[sources]` is absent entirely, **insert
   `[sources] default = OFFICIAL_SOURCE`** and report it (`added [sources]
   default = …`). A `--source NAME` that doesn't exist is an error listing
   the available names.
3. Decide the range:
   - `name@range` given → use it verbatim, **no catalog fetch**.
   - no range → load the source's catalog, find `name` (exact match) → range
     = latest `major.minor`. Not found → `no package "name" in <prefix> —
     try: ply search name` (exit 1). Source without catalog → error saying
     so and suggesting `ply add name@<version>`.
4. Write under `[dependencies]`:
   - default source → `name = "6.1"`
   - other source → `name = { source = "corp", version = "6.1" }`
   - table missing → create it **immediately after `[package]`**.
   - key exists → replace value; report `ffmpeg "6.0" → "6.1"`; if unchanged,
     say `ffmpeg = "6.1" already in ply.toml` and exit 0 without writing.
   - name needs quoting (contains `.`) → quoted key, like the website shows.
5. Print `added ffmpeg = "6.1" to ply.toml` then `run ply build to resolve
   and lock`. `add` never builds.

Write is atomic (temp file + rename). Comments, key order and whitespace
elsewhere in the file are preserved.

## 4. Publisher

`scripts/registry-push.mjs` `publishState()`: besides the root `state.json`,
write `<namespace>/state.json` for every namespace present in the ledger
(today: `ply/state.json`), containing only that namespace's packages with
`package_count` / `image_count` / `total_bytes` recomputed. Same
`cache-control: public, max-age=300`. `--state-only` republishes both.
The website keeps reading the root file.

## 5. Code layout

- `ply-core/src/catalog.rs` (new)
  - `pub const OFFICIAL_SOURCE: &str`
  - `#[derive(Deserialize)] Catalog { packages: Vec<Package> }`,
    `Package { namespace, name, description, license, homepage, versions }`,
    `ImageVersion { version: Version, img: String, arch: Option<String>, path: String, bytes: u64, pushed_at: String }`
    with `#[serde(default)]` everywhere except `name`, `version`, `img`.
  - `impl Source { pub fn catalog_location(&self) -> Result<CatalogLocation> }`
    where `CatalogLocation = Url(String) | Path(PathBuf)`; forges → `Err`.
  - `Catalog::load(&Source) -> Result<Catalog>` (http via the existing
    `http_get_string`, dir via `fs::read_to_string`).
  - `Catalog::search(&self, query: &str) -> Vec<&Package>` (ranked).
  - `Catalog::get(&self, name) -> Option<&Package>`.
  - `Package::latest() -> Option<&ImageVersion>`, `Package::range_of_latest() -> Option<String>`,
    `Package::arches_of_latest() -> Vec<&str>`.
- `ply-cli/src/commands/search.rs`: `SearchArgs` handling, `resolve_source()`
  (shared), `render_rows(&[Row]) -> String`, `render_versions(...)`, JSON.
- `ply-cli/src/commands/add.rs`: manifest edit via `toml_edit`; pure
  `fn apply_add(doc: &mut toml_edit::DocumentMut, name, range, source: Option<&str>) -> AddOutcome`
  so the edit logic is testable without touching disk.
- `ply-cli/src/cli.rs`: `Init(InitArgs)`, `Search(SearchArgs)`, `Add(AddArgs)`;
  all under "Build & validate" in help order, before/after `build`.
- `Cargo.toml` (workspace): `toml_edit = "0.22"`; `ply-cli/Cargo.toml` uses it.
- Docs: `docs/cli.md` (three entries), `docs/registries.md` (new "Catalog
  (`state.json`)" section next to "Version listing (`index.json`)"),
  `docs/image-format.md` (one line in the registry protocol block),
  `docs/docker.md` (`docker search` → `ply search` row), `docs/quickstart.md`
  (`ply init` to write the manifest, `ply add python3` for the dependency line).

## 6. Errors (exact wording)

- no catalog: `source <prefix> publishes no catalog (state.json) — browse https://plybox.sh/registry/ or pin a version`
- forge: `searching a forge source (<spec>) is not supported — pin a version`
- bad JSON: `<location>: invalid state.json: <serde error>`
- network: `fetching <url> failed (<ureq error>)`
- add, not found: `no package "<name>" in <prefix> — try: ply search <name>`
- add, bad --source: `no source "<name>" in [sources] (have: default, corp)`
- add, no manifest: `no ply.toml in <cwd>`

## 7. Testing

Unit (no network):
- `catalog_location` for https with/without placeholder, http, file, forge.
- ranking order and case-insensitivity; limit/truncation count.
- `latest`, `range_of_latest`, `arches_of_latest` incl. arch derived from `img`.
- minimal-schema catalog parses; full real `state.json` fixture parses
  (a trimmed copy of the live file checked into `ply-core/tests/fixtures/`).
- `Catalog::load` from a `file://` dir created with `tempfile`.
- `render_rows` alignment and truncation; `--json` shape.
- `apply_add`: new table placed after `[package]`; existing key replaced;
  unchanged → no write; quoted keys; table form with `--source`;
  `[sources]` inserted when absent; comments preserved (assert the
  round-tripped text).

Manual / e2e:
- `cargo run -- search ffmpeg --source file:///tmp/cat/{package}` with the
  live root `state.json` copied to `/tmp/cat/state.json`.
- After the user runs `registry-push.mjs --state-only`: `ply search ffmpeg`
  and `ply add ffmpeg` against the real registry, then `ply build .` on a demo.

## 8. `ply init`

```
ply init [--yes] [--force] [DIR]
```

Scaffolds `DIR/ply.toml` (DIR defaults to `.`). Refuses if the file exists
unless `--force`. When stdin is not a TTY it behaves as `--yes`.

**Detection** (offline; filesystem only), producing `Defaults`:

| found in DIR | runtime | entrypoint | port |
|---|---|---|---|
| `package.json` | `node` | `["node", <package.json "main", else "server.js">]` | 3000 |
| `requirements.txt`, `pyproject.toml`, or any `*.py` | `python3` | `["python3", "app.py"]` (`main.py` if that exists and `app.py` does not) | 8000 |
| otherwise | none | `["/bin/sh", "-c", "echo hello from ply"]` | none |

**Prompts** (default in brackets; Enter accepts; `--yes` accepts all):

1. `package name` — default: directory name lowercased, runs of anything
   outside `[a-z0-9-]` collapsed to `-`, trimmed; `app` if that is empty.
2. `version` — `0.1.0`; must parse as a semver `Version`.
3. `entrypoint` — detected default, shown and read as one line split on
   whitespace (no quoting parser; documented).
4. `base` — `alpine@<latest major.minor>`.
5. `runtime` — `python3 = "<latest>"` / `node = "<latest>"` / none; the
   answer is a range (`3.12`) or empty for none.
6. `port` — detected default or none; empty omits `[ports]`.

**Latest versions**: before prompting, load the catalog from
`OFFICIAL_SOURCE` once and take `range_of_latest()` for `alpine`, `python3`,
`node`. On any failure fall back to `3.20`, `3.12`, `22` and print
`note: could not reach the registry — using built-in defaults` on stderr.

**Output** — the quickstart's manifest shape, commented:

```toml
[package]
name = "myapp"
version = "0.1.0"
entrypoint = ["python3", "app.py"]
base = "alpine@3.20"
# include = ["dist/"]   # ship only these paths (default: everything in this directory)

[dependencies]
python3 = "3.12"

[ports]
http = 8000

[sources]
default = "https://registry.plybox.sh/ply/{package}"
```

`[dependencies]` / `[ports]` are omitted when empty. After writing, print the
file and:

```
wrote ply.toml
next: ply build .          # resolve, lock, build the image
      ply add <package>    # add a dependency from the registry
      commit ply.lock; ignore *.img
```

It touches nothing but `ply.toml`.

**Code**: `ply-cli/src/commands/init.rs` with pure, tested pieces —
`detect(dir) -> Defaults`, `prompt(&mut impl BufRead, &mut impl Write, &Defaults, yes) -> Result<Answers>`,
`render_manifest(&Answers) -> String` — and a thin `exec` that wires stdin/
stdout, the catalog fetch, and the file write (atomic temp + rename).

**Tests**: detection for node / python / none (tempdir); name sanitization
cases; `prompt` with scripted input (defaults accepted, overrides, bad
version re-asked); every `render_manifest` output passes `Manifest::parse`
and round-trips its fields; refuses to overwrite without `--force`.

Out of scope: `include` prompt, `.gitignore` edits, Go/Rust detection,
`ply new <dir>`.

## 9. Open follow-ups (not in this spec)

- Built-in default source in the resolver (would remove `[sources]` from
  every manifest; a separate decision).
- `ply remove`, `ply info`, `ply new`, multi-source search.
- Registry page on the website: show `ply add <name>` next to the dep line.
