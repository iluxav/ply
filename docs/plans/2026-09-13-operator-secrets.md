# Operator-Injected Secrets Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Let an operator set a runtime secret env var on a specific service from the dashboard (Heroku-style "config vars, some 🔒"), with the value stored host-only (never in the deployment spec, unit, or git) and injected at runtime — no app-manifest edit required.

**Architecture:** ply already has all the hard, security-critical machinery: a per-`member.param` `SecretStore` (0600, systemd-invisible), a `ResolvedEnv { secret: bool }` taint flag, and `split_env` which routes tainted values into a 0600 `--env-file` instead of the world-readable systemd unit. This feature adds a thin **operator lane** on top: a `secret_env = ["KEY", …]` list on a member/deployment names keys whose *values* come from the store; reconcile looks each up and feeds it into the existing tainted-env pipe. The dashboard writes the store files directly (it already manipulates `deployments/` files daemonlessly) and surfaces a Config Vars panel. This is deliberately **reconcile-side / prod-only** — `ply up` (dev) keeps using `--env-file`, which is the dev/prod split we want (env files = dev, store = prod).

**Tech Stack:** Rust (ply-core, ply-cli), Go (ply-dashboard), htmx templates.

**Decisions locked in** (from design discussion):
- Env-var name == store key (1:1, no `{…}`, no manifest decl). Cross-service sharing is out of scope — use the declared-param peer ref `{svc.param}` for that.
- Config Vars panel ships on **both** the cart (compose time) and the app page (post-deploy — the Heroku moment).
- "Delete + data" **auto-sweeps** the deployment's secret store.
- Dashboard writes store files **directly in Go** (matches the daemonless, file-based model) — it does NOT shell `ply`. The on-disk contract is frozen in `ply-core/src/secrets.rs::set` and must be matched byte-for-byte.

**Store on-disk contract (frozen — match exactly in Go):**
- Dir: `<deployments>/.secrets/<deployment>/`, mode `0700`.
- File: `<member>.<KEY>`, mode `0600`, content = `value` + `"\n"` (one trailing newline).
- Atomic write: temp `.<member>.<KEY>.tmp` in the same dir → `rename`.
- Read strips exactly one trailing `\n`.
- `list` = regular files in the dir (skip the `env/` subdirectory reconcile owns).

---

## Phase 1 — ply-core: the `secret_env` field

### Task 1: `secret_env` on the stack `Member`

**Files:**
- Modify: `ply-core/src/stack.rs` (`pub struct Member` ~line 66; the `parse`/member-reading code)
- Test: `ply-core/src/stack.rs` (tests module)

**Step 1: Write the failing test** — parsing a member with `secret_env`.
```rust
#[test]
fn member_parses_secret_env() {
    let stack = parse(
        "[[app]]\nrun=\"git+https://x/y\"\nname=\"api\"\nsecret_env=[\"STRIPE_KEY\",\"SENDGRID_KEY\"]\n",
        Path::new("d.toml"),
    ).unwrap().unwrap();
    assert_eq!(stack.members[0].secret_env, vec!["STRIPE_KEY", "SENDGRID_KEY"]);
}
```

**Step 2:** Run `cargo test -p ply-core member_parses_secret_env` → FAIL (no field).

**Step 3:** Add `pub secret_env: Vec<String>` to `Member`; parse the `secret_env` array in the member reader (default empty). Reject a key that isn't a valid env-var name (`[A-Za-z_][A-Za-z0-9_]*`) and reject a key that also appears in `env` (`"KEY is both env and secret_env — a value can't be public and secret"`).

**Step 4:** Run the test → PASS. `cargo test -p ply-core` stays green.

**Step 5:** Commit.

### Task 2: `secret_env` on `Spec`, carried by `from_stack_member`

**Files:**
- Modify: `ply-core/src/deployments.rs` (`pub struct Spec` ~line 129; `from_stack_member`)
- Test: `ply-core/src/deployments.rs`

**Step 1: Failing test** — a single-app deployment file parses `secret_env`, and `from_stack_member` copies a member's `secret_env` onto the synthesized `Spec`.
```rust
#[test]
fn spec_parses_and_carries_secret_env() {
    let s = Spec::parse("app=\"api\"\nsecret_env=[\"STRIPE_KEY\"]\n").unwrap();
    assert_eq!(s.secret_env, vec!["STRIPE_KEY"]);
}
```

**Step 2:** Run → FAIL.

**Step 3:** Add `#[serde(default)] pub secret_env: Vec<String>` to `Spec`. In `from_stack_member`, set `spec.secret_env = member.secret_env.clone()` on every arm that builds a Spec. `flags()` does NOT emit anything for `secret_env` (values are injected by reconcile, never argv). Confirm the "params-free member stays byte-identical" test still passes (an empty `secret_env` adds no flags).

**Step 4:** Run → PASS; `cargo test -p ply-core` green.

**Step 5:** Commit.

---

## Phase 2 — ply-core: `SecretStore::remove` + `ply secret rm`

### Task 3: `SecretStore::remove`

**Files:**
- Modify: `ply-core/src/secrets.rs`
- Test: `ply-core/src/secrets.rs`

**Step 1: Failing test** — set then remove; `get` returns `None`; removing a missing secret is Ok (idempotent).
```rust
#[test]
fn remove_deletes_and_is_idempotent() {
    let d = tempfile::tempdir().unwrap();
    let s = SecretStore { dir: d.path().into() }; // or a test ctor
    s.set("api", "STRIPE_KEY", "sk_live").unwrap();
    assert!(s.get("api", "STRIPE_KEY").unwrap().is_some());
    s.remove("api", "STRIPE_KEY").unwrap();
    assert!(s.get("api", "STRIPE_KEY").unwrap().is_none());
    s.remove("api", "STRIPE_KEY").unwrap(); // no error on missing
}
```

**Step 2:** Run → FAIL.

**Step 3:** Add `pub fn remove(&self, member, param) -> Result<()>` — `std::fs::remove_file`, treating `NotFound` as success, all other IO errors propagated.

**Step 4:** Run → PASS.

**Step 5:** Commit.

### Task 4: `ply secret rm` CLI

**Files:**
- Modify: `ply-cli/src/cli.rs` (`enum SecretCommand` ~1036; add `Rm(SecretRmArgs)` + `struct SecretRmArgs` mirroring `SecretSetArgs`' `-C`/`--deployments`/`name`)
- Modify: `ply-cli/src/commands/secret.rs` (add `exec_rm`; reuse `select_store` + `parse_name`)
- Modify: `ply-cli/src/commands/mod.rs` (wire `SecretCommand::Rm => secret::exec_rm(&args)`)
- Test: `ply-cli/src/commands/secret.rs`

**Step 1: Failing test** — `rm(&store, "api.STRIPE_KEY")` after `set` leaves `ls` empty; value never printed.

**Step 2:** Run → FAIL.

**Step 3:** Implement `exec_rm` (parse `member.param`, `store.remove`, print `removed <label>`). Never print the value.

**Step 4:** `cargo test -p ply-cli` → PASS; `cargo build` green.

**Step 5:** Commit.

---

## Phase 3 — ply-core: reconcile injects operator secrets

### Task 5: `EnvSource::OperatorSecret`

**Files:**
- Modify: `ply-core/src/stack.rs` (`pub enum EnvSource` ~line 1450; its `Display`)

**Step 1:** Add variant `OperatorSecret` (the store label, e.g. `"secrets/api.STRIPE_KEY"`, as a `String` payload for `ply why`/plan readability): `OperatorSecret(String)`, Display → `secret  {label}`. Build.

**Step 2:** `cargo build -p ply-core` → any non-exhaustive match now fails to compile; fix each match arm.

**Step 3:** Commit.

### Task 6: reconcile injection — stack path

**Files:**
- Modify: `ply-cli/src/commands/reconcile.rs` (the `for (member, mut spec, fetched) in pending` loop, just before `let (flags, file) = split_env(entries);` ~line 630)
- Test: `ply-cli/src/commands/reconcile.rs`

**Step 1: Failing test** — a helper that, given `spec.secret_env = ["STRIPE_KEY"]` and a store holding `api.STRIPE_KEY=sk_live`, produces an augmented entries list where `STRIPE_KEY` is present with `secret: true`, and `split_env` routes it to the file, not the flags. (Factor the injection into a pure, testable fn `fn inject_operator_secrets(entries: Vec<ResolvedEnv>, secret_env: &[String], member: &str, store: &SecretStore) -> Result<Vec<ResolvedEnv>>`.)
```rust
#[test]
fn operator_secret_is_injected_tainted_and_hidden_from_flags() {
    let d = tempfile::tempdir().unwrap();
    let store = /* SecretStore at d */;
    store.set("api", "STRIPE_KEY", "sk_live").unwrap();
    let entries = inject_operator_secrets(vec![entry("NODE_ENV","production",false)],
        &["STRIPE_KEY".into()], "api", &store).unwrap();
    let (flags, file) = split_env(&entries);
    assert!(!flags.iter().any(|(k,_)| k=="STRIPE_KEY"));
    assert_eq!(file, vec![("STRIPE_KEY".into(), "sk_live".into())]);
}
```

**Step 2:** Run → FAIL.

**Step 3:** Implement `inject_operator_secrets`: for each `k` in `secret_env`, `store.get(member, k)?` →
- `Some(v)` → push `ResolvedEnv { key: k, value: v, secret: true, source: EnvSource::OperatorSecret(store.label(member, k)) }`.
- `None` → return `Err` naming the KEY and the `ply secret set` fix (never the value). A key already present in `entries` (collision) → `Err`.
Then wire it into the pending loop: `let entries = inject_operator_secrets(entries.to_vec(), &spec.secret_env, &member, &secrets)?;` before `split_env`. On `Err`, mark THAT member failed (write_status false + `deploy-failed` event) and `continue` — peers keep converging (same shape as a member-level failure already in this loop).

**Step 4:** Run → PASS; `cargo test` green.

**Step 5:** Commit.

### Task 7: reconcile injection — single-app path

**Files:**
- Modify: `ply-cli/src/commands/reconcile.rs` (locate the non-stack branch that builds a `Spec` and calls `apply`; there is no `split_env` there today because single-app specs had no secrets)

**Step 1: Failing test** — a single-app deployment `api.toml` with `secret_env=["STRIPE_KEY"]` and a store `SecretStore::for_deployments("api")` holding `api.STRIPE_KEY` results in a 0600 env-file injected and the value absent from the unit flags. (Integration-style; if the single-app apply isn't unit-testable, add a focused test around the extracted injection fn with `member == deployment name`.)

**Step 2:** Run → FAIL.

**Step 3:** In the single-app apply, when `spec.secret_env` is non-empty: build entries from `spec.env` (all `secret: false`), run `inject_operator_secrets(entries, &spec.secret_env, <deployment name>, &SecretStore::for_deployments(<name>))`, `split_env`, `write_member_secrets_file(<name>, <name>, &file)` → set `spec.env_file`/`spec.env` accordingly before `apply`. Store key member == the deployment name for single-app.

**Step 4:** Run → PASS; full `cargo test` + `cargo clippy` green.

**Step 5:** Commit.

### Task 8: `ply why` / status surfacing (small)

**Files:**
- Modify: `ply-core/src/why.rs` if it enumerates env sources (so an operator secret shows as `secret  secrets/api.STRIPE_KEY`, value masked).

**Step 1–4:** If `why` already renders `EnvSource`, the new variant is covered by Task 5's Display; add/adjust a test asserting the value is masked. Otherwise skip with a note.

**Step 5:** Commit.

---

## Phase 4 — ply-core release

### Task 9: changelog + release

**Files:** `CHANGELOG.md`, version bump per the repo's release flow.

- Add a CHANGELOG entry: "operator secrets: `secret_env = [\"KEY\"]` on a deployment/member injects a host-stored value at runtime (0600, never in the spec or unit); `ply secret rm`."
- Release, wait for CI, verify the new `ply` rolls onto the QA droplet (`64.23.252.143`).
- **Do not** touch production (`64.23.144.72`).

---

## Phase 5 — dashboard: cart models `secret_env` losslessly

### Task 10: `SecretEnv` on the cart `Card`

**Files:**
- Modify: `ply-dashboard/internal/cart/cart.go` (`type Card`; `FromTOML`; `ToTOML`/`member()`/`flat()`)
- Test: `ply-dashboard/internal/cart/cart_test.go`

**Step 1: Failing test** — round-trip a `[[service]]` with `secret_env = ["STRIPE_KEY"]`: `FromTOML` populates `Card.SecretEnv`; `ToTOML` re-emits it; a deployment with un-modeled fields still round-trips (lossless).

**Step 2:** Run `go test ./internal/cart/` → FAIL.

**Step 3:** Add `SecretEnv []string` to `Card`; parse `secret_env` in `FromTOML`; render it in `member()`/`flat()` (after `env`). Ensure it doesn't collide with the lossless `Extra` passthrough (model it explicitly so it's not double-emitted).

**Step 4:** Run → PASS; `go test ./...` green.

**Step 5:** Commit.

---

## Phase 6 — dashboard: Go secret-store wrapper (file-direct)

### Task 11: `plystate` secrets store

**Files:**
- Create: `ply-dashboard/internal/plystate/secrets.go`
- Test: `ply-dashboard/internal/plystate/secrets_test.go`

**Step 1: Failing test** — `SetSecret(p, dep, member, key, value)` writes `<Deployments>/.secrets/<dep>/<member>.<key>` mode 0600 (dir 0700) with content `value+"\n"` via temp+rename; `SecretNames(p, dep)` lists `member.key` from regular files, skipping the `env/` subdir; `RemoveSecret` deletes and is idempotent. Assert perms and byte-exact content (must match `ply secret ls`/reconcile).
```go
func TestSecretStoreRoundTrip(t *testing.T){ /* set, names, remove; assert 0600, "\n" */ }
```

**Step 2:** Run → FAIL.

**Step 3:** Implement matching the frozen contract exactly (validate names `[A-Za-z_][A-Za-z0-9_]*` for key, `[a-z0-9-]` for member/dep; refuse path traversal). Never log values.

**Step 4:** Run → PASS.

**Step 5:** Commit.

---

## Phase 7 — dashboard: Config Vars panel

### Task 12: endpoints (set / list / remove)

**Files:**
- Modify: `ply-dashboard/main.go` (routes + handlers, guarded)
- Modify: `ply-dashboard/deploy_builder.go` (cardView gains config-var rows: plain env ∪ secret_env, each tagged; secret rows show `set`/`not set yet` from `SecretNames`)
- Test: `ply-dashboard/*_test.go`

**Step 1: Failing test** — POST secret-set adds the key to the card's `secret_env` (draft round-trip) AND writes the store file under the draft/deployment name; the response re-renders the panel showing `•••• set`; the value is never echoed back. POST secret-remove drops the key from `secret_env` and removes the store file.

**Step 2:** Run → FAIL.

**Step 3:** Add:
- `POST /deploy/draft/{id}/card/{i}` new ops `secret-set` (key,value → `SetSecret(p, id, member, key, value)` + add key to `Card.SecretEnv`) and `secret-rm` (→ `RemoveSecret` + drop from `SecretEnv`). Draft id == deployment name, so the store path is correct pre-deploy.
- `POST /app/{name}/secret/set` and `/app/{name}/secret/remove` for the post-deploy app page (resolve the deployment + member via `plystate.DeploymentOf`), re-rendering the app page's config-var partial.
- Panel data: `cardView`/`pageData` expose config-var rows = plain `env` keys (value shown) ∪ `secret_env` keys (value hidden, `set`/`not set` from `SecretNames`).

**Step 4:** Run → PASS; `go vet`/`go test ./...` green.

**Step 5:** Commit.

### Task 13: templates — the panel (cart + app page)

**Files:**
- Modify: `ply-dashboard/web/templates/cart.html` (add a "config vars" block per card below the wire rows: KEY | value-or-`••••` | 🔒 toggle | ×, and an "+ add var" with a secret checkbox)
- Create: `ply-dashboard/web/templates/app_config.html` (`{{define "app-config"}}` partial mirroring `app_domains.html` — htmx-swapped `#app-config`, loading indicator, secret rows masked)
- Modify: `ply-dashboard/web/templates/app.html` (add the config-vars section for non-builder deployable apps, like the domains section)
- Modify: `ply-dashboard/main.go` (register `app-config` partial; app page passes config-var data)

**Step 1–4:** Wire htmx in-place swaps (reuse the domain pattern: `hx-target` the section, `htmx-indicator`, no full reload — consistent with the scroll-preservation guard already in `base.html`). A 🔒 add prompts for a value (masked input), submits to secret-set; toggling 🔒 off on an existing plain var is a no-op with a hint (can't retroactively hide a value already in git — set it fresh as secret and remove the plain one). Match the terminal-dark idiom.

**Step 5:** Commit.

---

## Phase 8 — dashboard: lifecycle

### Task 14: delete + data sweeps the secret store; rename migrates

**Files:**
- Modify: `ply-dashboard/internal/plystate/deploy.go` (the delete/"delete + data" path) — after removing the deployment + volumes, `os.RemoveAll(<Deployments>/.secrets/<dep>)`.
- Modify: the cart card-rename op — when a member is renamed, migrate its store files `<old>.KEY → <new>.KEY` (best-effort; if the migration fails, keep the old and surface a warning) so secrets don't orphan.
- Test: cover both (delete removes the dir; rename moves the files).

**Step 1–4:** TDD each. Ensure delete-of-a-live-deployment never touches another deployment's `.secrets/`.

**Step 5:** Commit.

---

## Phase 9 — dashboard release + droplet QA

### Task 15: deprecate seal/env-file in the prod flow (light touch)

**Files:** `ply-dashboard/web/templates/deploy.html` and helppane — demote the "seal" tab and "env files" tab to a dev/advanced note pointing at Config Vars as the prod path. Do not delete them.

### Task 16: release + verify on the droplet

- `make release` (bump, tag, CI builds + publishes; droplet self-updates on the reconcile beat).
- Verify on `64.23.252.143` end-to-end with `rtrtrtr`:
  1. On qa-server's card, add `STRIPE_KEY` as 🔒 with a test value.
  2. Confirm `deployments/rtrtrtr.toml` gains `secret_env = ["STRIPE_KEY"]` and **no value**; `deployments/.secrets/rtrtrtr/qa-server.STRIPE_KEY` exists 0600.
  3. After the beat, `ply exec qa-server env | grep STRIPE_KEY` shows it injected; the systemd unit does NOT contain the value; `/run` logs don't leak it.
  4. Rotate via the panel → rolling restart picks up the new value.
  5. Remove via the panel → key gone from spec + store; next beat drops it.
  6. Delete + data → `.secrets/rtrtrtr/` gone.
- Update the QA runbook `ply/docs/plans/2026-09-13-stack-qa.md` and the `ply-positioning-next-bet` memory if the secrets story changes the pitch.

---

## Out of scope (explicit)
- Cross-service sharing of one secret value (use the declared-param peer ref `{svc.param}`).
- `ply up` (dev) honoring `secret_env` — dev uses `--env-file` by design.
- Reading a secret value back in the UI — impossible by design (set/rotate/remove only).
- Migrating existing `enc:v1:` sealed values or `.env/*.env` files into the store (leave both working for their niches; just stop steering prod at them).
