# deb2pkg — Debian package → ply keg converter

**Date:** 2026-08-25
**Status:** approved design, pre-implementation
**Decision record:** Debian (trixie) becomes ply's default base lane; Alpine is frozen, not deleted. Wolfi rejected (vendor-curated free repo; redis already paywalled). Decided 2026-08-25 after measured comparison of all three.

## Purpose

Convert Debian packages (+ their runtime library closure) into ply kegs, and mint
a glibc Debian base package — so ply apps run on the libc that every ecosystem's
prebuilt artifacts target. Kills the musl tax (glibc `.node` addons, JNI
libraries, `--libc=musl` CI ceremony) at the root instead of warning about it.

Companion tool to `apk2pkg`, which stays as-is serving the frozen Alpine lane.

## Measured facts this design rests on

Spike results (2026-08-25, trixie main, ELF-closure walk prototype):

| keg | naive Depends walk | ELF closure | Alpine equivalent |
|---|---|---|---|
| memcached | 53 MiB (drags perl) | **1.7 MiB** | 1.3 MiB |
| nginx | 2.8 MiB | **2.4 MiB** | 1.5 MiB |
| redis (server+tools) | 9.3 MiB | **9.2 MiB** | 3.5 MiB |
| postgresql-17 | 340 MiB | **105 MiB** (no JIT) / 257 (JIT) | 30 MiB |
| nodejs | 100 MiB | **93 MiB** | 50 MiB |

Three rules the spike surfaced, all reflected below:
1. ELF-only misses binaries reached via symlinks across packages
   (Debian `redis-server` is a 0.2 MiB shell; binaries live in `redis-tools`).
2. dlopen'd plugins need an explicit skip (postgres `llvmjit.so` alone → +150 MiB).
3. soname→package resolution must prefer runtime lib packages over `-dev`/`-dbg`
   (naive pick hands `libLLVM.so` to `llvm-19-dev`, 339 MiB, instead of
   `libllvm19`, 124 MiB).

## Scope (locked)

- **In:** `deb2pkg` binary; `scripts/build-base-debian.sh`; proof kegs
  (redis, postgresql17, node) verified running locally on the debian base.
- **Out:** registry publishing/namespacing, third-party apt repos (`--repo`,
  NodeSource/PGDG), the ~130 first-class set, arm64 verification (flag exists,
  proof is x64), any changes to `apk2pkg` or the Alpine catalog.

## CLI

```
deb2pkg <package> [--suite trixie] [--arch x64|arm64] [--name N]
        [--outdir D] [--base 13] [--skip-so FILE.so]... [--refresh]
```

- `<package>`: Debian binary package name (`redis-server`, `postgresql-17`, `nodejs`).
- `--suite`: Debian suite, default `trixie`. Mirror fixed to `deb.debian.org/debian`, component fixed to `main`.
- `--arch`: ply arch; maps x64→`amd64`, arm64→`arm64` (index/Contents paths) — conversion is arch-independent unpacking, any host converts for any arch (same as apk2pkg).
- `--name`: override ply package name (default: sanitized).
- `--base`: base constraint the keg declares, default `13`.
- `--skip-so`: repeatable; ELF files whose NEEDED entries are NOT walked and which are dropped from the staged keg. Documented recipe: `deb2pkg postgresql-17 --skip-so llvmjit.so`.
- `--refresh`: force re-download of cached indexes regardless of age.

## Architecture

New workspace member `deb2pkg/` (add to `Cargo.toml` members), single
`src/main.rs` mirroring apk2pkg's structure. New workspace deps:
- `lzma-rs` (pure Rust): `Packages.xz`, `data.tar.xz`
- `zstd` (already built transitively via backhand): `data.tar.zst`
- existing: `flate2` (`Contents-*.gz`, `data.tar.gz`), `tar`, `ureq`, `semver`, `clap`, `anyhow`, `ply-core`

### Pipeline

```
1  fetch + cache indexes
     ~/.cache/deb2pkg/<suite>-<arch>/Packages.xz      (~10 MB)
     ~/.cache/deb2pkg/<suite>-<arch>/Contents.gz      (~12 MB)
     ~/.cache/deb2pkg/<suite>-<arch>/base.manifest    (debuerreotype rootfs.manifest)
     Cache is refreshed when older than 24h (mtime check), or on --refresh.

2  parse
     Packages: RFC822 stanzas → name → {version, depends, pre-depends,
       provides, source, filename, installed-size}. Continuation lines
       (leading space) are folded. Multiple stanzas per name: keep highest
       version (dpkg --compare-versions ordering, simplified: split on
       [.:+~-] with numeric/alpha comparison — good enough for main).
     Contents: "path  section/pkg[,...]" → basename→candidate-packages map,
       kept only for basenames matching \.so(\.\d+)*$.
     base.manifest: "pkg<TAB>version" lines → the base-provided package set.

3  seed set
     requested package
     + Depends/Pre-Depends targets sharing the same Source: field
       (one level; catches redis-server → redis-tools)
     Virtual names resolve via Provides (first provider). Alternatives
     (a | b): first alternative that exists in the index.

4  unpack loop (over a work queue of packages)
     download .deb → parse `ar` archive (fixed 60-byte headers; members
       debian-binary, control.tar.*, data.tar.*)
     extract ONLY data.tar.{xz,zst,gz} into the shared staging dir —
       control.tar (maintainer scripts) is never read: "no hooks, ever"
       holds by construction.
     entry handling as apk2pkg: skip char/block/fifo devices, widen
       unreadable modes (0o700 dirs / 0o400 files).

5  ELF walk (per unpacked package's files)
     For every regular file: read 4-byte magic; if ELF64, parse program
       headers → PT_DYNAMIC → DT_NEEDED indices + DT_STRTAB → soname strings.
       Own parser, ~150 lines, no readelf/goblin dependency. ELF32 files:
       warn + skip (Debian amd64/arm64 are 64-bit).
     Files named by --skip-so: removed from staging, not walked.
     Each NEEDED soname resolves:
       a. Contents map → candidate packages → score: penalize -dev/-dbg
          (+100), non-lib prefix (+10), then shortest name wins.
       b. winner in base.manifest set (libc6, zlib1g, libssl3t64, …) → skip.
          (This subsumes a hardcoded glibc soname list: ld-linux, libc.so.6
          etc. all resolve to libc6. A tiny fallback set {libc.so.6,
          ld-linux-x86-64.so.2, ld-linux-aarch64.so.1} guards Contents gaps.)
       c. winner already vendored or queued → skip; else enqueue → step 4.
     Unresolvable sonames: collected, printed as one warning block at the
       end (not fatal — mirrors apk2pkg's missing-dep warning).

6  emit keg
     staging → ply.toml:
       [package] name, version, provides_abi = "linux-<arch>-gnu"
       [layer] path = existing of {usr/bin, bin, usr/sbin, sbin}
               ld_library_path = every staged dir containing a *.so* file,
               usr/lib + lib first, then the rest (multiarch
               usr/lib/x86_64-linux-gnu lands here naturally)
       [dependencies] debian = "<--base>"
     ply_core::build::build() → <name>-<version>-linux-<arch>.img
     (same call as apk2pkg; deterministic squashfs).
```

### Version and name normalization

- **Epoch strip:** `5:8.0.2-1` → `8.0.2` (split on `:`, take last part, then
  semverize as apk2pkg does: leading digits/dots, zero-pad to three parts).
- **Name sanitize:** apk2pkg's rules (lowercase, `+`→`p`, `_`→`-`) **plus**
  digit-suffix folding: `postgresql-17` → `postgresql17` (ply names reject
  `-<digit>`).

## Debian base package

`scripts/build-base-debian.sh <arch> [outdir]` — same shape as `build-base.sh`:

- Source (verified 2026-08-25): the debuerreotype artifacts repo.
  - rootfs: `https://raw.githubusercontent.com/debuerreotype/docker-debian-artifacts/dist-<amd64|arm64v8>/trixie/slim/oci/blobs/rootfs.tar.gz` (~30 MiB)
  - integrity: sha256 of the tarball must equal the layer digest in
    `…/oci/blobs/image-manifest.json` (this digest is byte-identical to the
    base layer inside `postgres:16` et al. — measured 2026-08-24)
  - version: `…/trixie/slim/rootfs.debian_version` (currently `13.6`)
  - package list: `…/trixie/slim/rootfs.manifest` (78 packages) — the same
    file deb2pkg caches as `base.manifest`
- Extract minus `dev/*`, write ply.toml:
  `name = "debian"`, `version = <debian_version>.0` (semverized, e.g. 13.6.0),
  `base = true`, `provides_abi = "linux-<arch>-gnu"`, then `ply build`,
  rename for target arch (host-arch naming quirk handled as in build-base.sh).
- The resolver's existing ABI guard (resolve.rs) then refuses musl kegs on the
  debian base at build time — mixing fails loudly, by design.

## Error handling

- Package not found: fail with the name + suite, suggest close matches
  (prefix match over index keys, max 5).
- Download failures: fail the run (no partial kegs); cached indexes make
  retries cheap.
- `.deb` member with unknown compression: fail naming the member and package.
- Unresolvable sonames: warn-and-continue (collected block), exit 0 — the keg
  may still work (dlopen'd optionals); the warning names each soname and the
  file that needed it.
- ELF32 / non-ELF with `.so` name: skip silently (data files named *.so exist).

## Testing

TDD; unit tests in `deb2pkg/src/main.rs` mirroring apk2pkg's:

- epoch strip + semverize (`5:8.0.2-1`→8.0.2, `1:2.41.5-0+deb13u1`→2.41.5, `20230311`→20230311.0.0)
- name sanitize incl. digit-suffix folding (`postgresql-17`→postgresql17, `libstdc++6` handling)
- RFC822 stanza parser: folding, multi-stanza highest-version pick, Provides/alternatives
- `ar` parser on a byte-literal fixture
- DT_NEEDED extraction on a fixture ELF (small hand-assembled or `include_bytes!` of a tiny .so built once and committed)
- soname scoring (libllvm19 beats llvm-19-dev; libpcre2-8-0 beats libpcre2-dev)
- Contents line parsing (basename filter, multi-package cells)
- arch mapping (x64→amd64, arm64→arm64v8 branch / arm64 index)

Plus `cargo clippy -D warnings` and the musl release build, per the standing
verification loop.

## Verification gate (user-run, no sudo expected; rootless ply)

1. `scripts/build-base-debian.sh x64 ./out` → `debian-13.6.0-linux-x64.img`
2. `deb2pkg redis-server --name redis` → redis keg; app manifest
   `entrypoint = ["redis-server", "--protected-mode", "no"]`,
   `[dependencies] debian + redis`; `ply run` → `redis-cli PING` (or
   `printf '*1\r\n$4\r\nPING\r\n' | nc`) answers `PONG`.
3. `deb2pkg postgresql-17 --name postgresql17 --skip-so llvmjit.so` →
   initdb + `select version()` via the documented pgdb-style manifest.
4. `deb2pkg nodejs` → `node -e 'console.log(process.version)'` AND loading a
   host-built glibc native addon (the motivating case — e.g. a `.node` file
   from any host `npm i` of `better-sqlite3` or `sharp`) succeeds.

Gate: all four pass rootless on the dev machine. Registry untouched.

## Risks / notes

- **Debian versions in ply.toml are lossy** (epoch + revision stripped);
  acceptable — apk2pkg made the same trade. The registry ledger keys keep the
  full upstream version string when publishing eventually happens.
- **`Contents.gz` basename collisions** (same soname shipped by unrelated
  packages) are resolved by scoring; wrong picks surface as oversized kegs in
  review, not silent breakage — the closure is printed per run.
- **debuerreotype repo layout changed once already** (tarball moved into
  `oci/blobs/`); the base script pins exact URLs and fails loudly, and the
  sha256-vs-manifest check catches content drift.
- **dlopen closures are invisible to the ELF walk** (e.g. nss modules,
  gconv). If a proof keg hits one at runtime, the fix is vendoring the extra
  package explicitly — add a `--with <pkg>` repeatable flag at that point,
  not preemptively (YAGNI).

## Implementation amendments (2026-08-25, discovered during the proof gate)

- **`--with <pkg>` exists** (repeatable, no dependency recursion): the node
  proof hit the predicted case — `libnode115` depends on pure-JS data
  packages (node-cjs-module-lexer, node-undici, node-acorn, node-minimatch)
  the ELF walk can't see.
- **Multilib guard**: Contents entries under `usr/lib32`/`usr/libx32` are
  ignored and `lib32*`/`lib64*`/`libx32*` package names are score-penalized —
  without this, `libreadline.so.8` resolved to `lib32readline8` (32-bit)
  instead of `libreadline8t64` because t64 names are longer.
- **Debian's `nodejs` is relocation-hostile and stays unconverted**: it
  externalizes builtins at compiled-in absolute paths
  (`/usr/share/nodejs/…`), and since ply kegs everything under
  `/opt/<name>-<version>` (apps too — the no-shadowing guarantee), no image
  can satisfy such paths. The lane's node keg is minted from nodejs.org's
  official glibc tarball instead (self-contained, current — v24 vs Debian's
  v20): tarball → staging + package ply.toml → `ply build`. Verified:
  node v24.6.0 on the debian base loads a host-built glibc lightningcss
  `.node` addon.
- **Verification results**: debian base 13.6.0 (25.7 MiB, sha-verified);
  redis 8.0.2 keg 1.9 MiB → PONG rootless; postgresql17 17.10 keg 40.9 MiB
  → `select version()` rootless (binaries at
  `usr/lib/postgresql/17/bin` — Debian's `usr/bin` wrappers are perl and
  deliberately not vendored); node addon case above. 169 workspace tests,
  clippy/fmt clean.
