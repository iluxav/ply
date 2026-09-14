# Build Stage Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make `ply build` produce correct, lean Linux images regardless of the host it runs on — never packing host-native (e.g. macOS) `node_modules` into a Linux image — by (A) refusing foreign-arch native addons at pack time, and (B) giving the manifest a `[build] command` that ply runs inside a builder image before packing, unifying `ply build` with the CD path.

**Architecture:** The CD path (`build_from_repo` in `ply-cli/src/commands/reconcile.rs`) already does this right — it synthesizes a builder image (`base` + runtime), runs the build command inside it with the source mounted at `/work`, then packs. `ply build` (via `ply_core::build::build`) does NOT: it packs the directory as-is. This plan (A) adds a cheap safety net so a foreign-arch native addon is a loud refusal instead of a silent runtime break, then (B) lifts the build command into the manifest and runs it in a builder image for `ply build` too, refactoring `build_from_repo` to share that machinery so local and CD builds are identical.

**Tech Stack:** Rust (ply-core, ply-cli).

**Context already established:**
- `ply_core::build::build(BuildOptions)` — `arch = opts.arch.unwrap_or_else(Arch::host)`; `audit_tree(&opts.dir, &filter, &include)` → `(packed_files, packed_bytes, secrets)` is the exact set that ships.
- `Arch` (`ply-core/src/image/name.rs`): `X64` (`as_str()` → "x64") and the arm variant; images are always Linux (`Os`).
- `build_from_repo` builds a `<name>-builder` image (`base = "debian@13"`, `[dependencies] <rt>`), runs it with the checkout at `/work` via `ply_core::runtime::run::run`, then packs the checkout. #1 (runtime pinning) already shipped in v0.1.108 via `repo_runtime`.
- `Manifest::parse` + `[build]` grouped section already exists (base, dependencies, include, sources). Adding `command` there is natural.

---

## Phase A — refuse foreign-arch native addons at pack time

### Task A1: classify a binary's platform/arch

**Files:**
- Create: `ply-core/src/nativeaddon.rs` (or a small module in `build.rs`)
- Test: same file

**Step 1: failing test** — `classify(bytes) -> AddonKind` where AddonKind is `ElfX64 | ElfArm64 | MachO | Pe | Unknown`:
```rust
#[test]
fn classifies_binary_headers() {
    assert_eq!(classify(&[0x7f,b'E',b'L',b'F',2,1,1,0,0,0,0,0,0,0,0,0,2,0,0x3e,0]), AddonKind::ElfX64);
    assert_eq!(classify(&[0x7f,b'E',b'L',b'F',2,1,1,0,0,0,0,0,0,0,0,0,2,0,0xb7,0]), AddonKind::ElfArm64);
    assert_eq!(classify(&[0xcf,0xfa,0xed,0xfe, 0,0,0,0]), AddonKind::MachO); // Mach-O 64 LE
    assert_eq!(classify(&[0xca,0xfe,0xba,0xbe, 0,0,0,0]), AddonKind::MachO); // fat
    assert_eq!(classify(&[b'M',b'Z',0,0]), AddonKind::Pe);
    assert_eq!(classify(&[0,1,2]), AddonKind::Unknown);
}
```

**Step 2:** run → FAIL.

**Step 3:** implement. ELF: magic `\x7fELF`; e_machine is a u16 at offset 18 (respect EI_DATA at byte 5 for endianness — linux x64/arm64 are LE), `0x3E`→X64, `0xB7`→Arm64. Mach-O: `FEEDFACE`/`FEEDFACF` (either endianness) and fat `CAFEBABE`/`BEBAFECA`. PE: `MZ`. Else Unknown.

**Step 4:** run → PASS.

**Step 5:** commit.

### Task A2: scan the packed set, refuse a mismatch

**Files:**
- Modify: `ply-core/src/build.rs` (after `audit_tree`, before packing)
- Test: `ply-core/src/build.rs`

**Step 1: failing test** — a temp dir with `include = ["mod.node"]` where `mod.node` is a Mach-O header, built for `Arch::X64`, fails with an error naming the file and the arch; an ELF-x64 `.node` for `Arch::X64` builds fine; a pure-JS tree (no `.node`) is unaffected.

**Step 2:** run → FAIL.

**Step 3:** add `fn check_native_addons(dir, packed_files, target: Arch) -> Result<()>`: for each packed file ending in `.node`, read the first ~20 bytes, `classify`; refuse when positively foreign — `MachO`/`Pe`, or `Elf*` whose arch ≠ target; allow the matching Elf; skip `Unknown` (conservative — never a false refusal). Error: ``native addon `<path>` is a <kind> binary, but this image targets linux-<arch> — its node_modules were built on a different platform. Declare a `[build]` step so ply builds them in the image, or build node_modules for linux-<arch>, then rebuild.`` Call it from `build()` after `audit_tree`.

**Step 4:** run → PASS; `cargo test -p ply-core` green.

**Step 5:** commit.

### Task A3: release A as immediate safety

- CHANGELOG `## Unreleased`: "ply build now refuses foreign-arch native addons instead of shipping a broken image."
- `env -u PLY_MICROVM_KERNEL make release-cli`; droplet `ply self-update`.

---

## Phase B — `[build] command` run in a builder image

### Task B1: parse `[build] command` in the manifest

**Files:**
- Modify: `ply-core/src/manifest.rs` (the `[build]` group; add `command: Option<String>` — or `Vec<String>` steps, joined with `&&`)
- Test: `ply-core/src/manifest.rs`

**Step 1: failing test** — `Manifest::parse` reads `[build] command = "npm ci && npm run build"` into the field; absent → None; the flat form still parses.

**Step 2–4:** add the field (serde, grouped + flat), TDD.

**Step 5:** commit.

### Task B2: a shared builder-stage helper

**Files:**
- Create/relocate: a `build_stage` helper — the builder-image synthesize+run currently inline in `build_from_repo`. Extract it (into `ply-core` so `ply build` can call it, or a shared `ply-cli` fn if `ply build` lives there) so both paths use one implementation. Signature roughly `run_build_stage(src: &Path, base: &str, runtime: &str, command: &str, mem, env) -> Result<()>`.
- Test: unit-test what's testable (the builder manifest text; the runtime pin from #1).

**Step 1–5:** extract with the runtime pin (`repo_runtime` logic generalized), memory fence, node heap/cache env, TMPDIR — all already in `build_from_repo`. TDD the manifest synthesis; the actual run stays integration (needs the runtime backend). Commit.

### Task B3: `ply build` runs the build stage when `[build] command` is present

**Files:**
- Modify: the `ply build` command (`ply-cli/src/commands/*`) and/or `ply_core::build::build`
- Test: integration-style / manifest-synthesis unit

**Step 1: behavior** — when the manifest has `[build] command`, `ply build`: synthesize the builder image (this `base` + these `dependencies`), run `command` with the source mounted, THEN pack (existing pack path). On Linux → namespace sandbox; on macOS → microVM (document the kernel dependency). When absent → today's pack-as-is (plus the A-scan backstop).

**Step 2–5:** implement using B2's helper; TDD the "command present → build stage runs → pack" wiring where feasible. Commit.

### Task B4: refactor `build_from_repo` onto the shared helper

**Files:**
- Modify: `ply-cli/src/commands/reconcile.rs`

Make `build_from_repo` call B2's helper so CD and `ply build` are one path. The deployment `build=` maps to the manifest `[build] command`. Keep behavior identical (the existing reconcile tests must stay green). Commit.

---

## Phase C — polish

### Task C1: `ply init` writes a default `[build] command` for detected node/framework projects, and the lean-output convention (`npm ci && npm run build && npm prune --omit=dev`, tight `include`). Document in `references/manifest.md`. Optional: surface `[build] command` in the dashboard cart card (read-only or editable).

---

## Phase D — release + QA
- CHANGELOG, `make release-cli`, droplet self-update.
- QA on the droplet: a repo with a native addon (e.g. `bcrypt`) + a `[build] command` builds correctly and runs; without a build command, packing host-foreign node_modules is refused with the A-scan message. Verify the CD path (a `git+` member) still builds identically after the B4 refactor.

## Non-goals
Dockerfile builds, arbitrary/foreign (non-Linux) base images, remote build caching, inferring the build command from `package.json` scripts (explicit `[build] command`, no magic).
