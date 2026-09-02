# Params & Wiring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One params namespace replaces hand-threaded env: providers declare `[params]`, consumers write `{app.param}` in stack env values, secrets are minted 0600 files, live state is a `/run/ply` file tree apps can wait on, and `ply up --plan` shows every resolved value with its source.

**Architecture:** A new pure module `ply-core/src/params.rs` (declarations, template engine, namespaces, built-in facts) plus `ply-core/src/secrets.rs` (file store, minting). `ply up` gains a resolution pass that builds one `Namespace` per member and resolves `{}` holes at spawn time — inside the stack netns every fact is static (`<name>.ply` + container port), so nothing needs late binding in the child. The run parent materializes live state under `run_dir()/params/<app>/`, bind-mounted into containers at `/run/ply`; `--after` grows an equality-only condition grammar over those files.

**Tech Stack:** Rust 2021 workspace (`ply-core` lib, `ply-cli` bin), `toml`/`serde`, `thiserror` core / `anyhow` CLI, `tempfile` (dev-dep, already present). No new dependencies — random bytes come from `/dev/urandom`.

**Spec:** `docs/superpowers/specs/2026-09-01-params-wiring-design.md`

## Global Constraints

- **Never commit.** The repository owner commits; each task ends with `make check` green (fmt + clippy `-D warnings` + workspace tests) and the tree left for them. (Project rule.)
- Reserved param names, verbatim: `name version host port addr base_url scale arch image state instances started_at restarts self`. Live subset: `state instances started_at restarts`.
- Wait grammar is exactly three forms: `app`, `app.param`, `app.param == 'literal'` (single or double quotes). **No other operator exists.**
- Template escapes: `{{` → `{`, `}}` → `}`. `$VAR` expansion is untouched and runs first (existing `stack::expand_vars`).
- `{}` holes in a **manifest** are interpreted only when the manifest has a `[params]` table (even empty). Stack `e =` values and member `params` overrides always interpret `{}`.
- Secret mask string everywhere: `********`. Minted secrets: 32 chars from `[A-Za-z0-9]` (URL-safe by construction). Secret files: mode `0600`, tmp-write + atomic rename.
- Resolution happens in `ply up` before spawning; children receive final values. Secret-tainted values pass via the child's process environment + bare `-e KEY` (never in argv).
- Error conventions: parse/validate → `Error::Manifest(String)`, runtime → `Error::Runtime(String)`, both with a remedy sentence. Wait timeout message shape, verbatim: `waiting for server.finish_boot == 'ok' (currently unset, 30s elapsed)`.
- Unit tests live in-file under `#[cfg(test)]`, sentence-shaped names, TOML string literals + `tempfile::tempdir()`, never the network.
- Everything is additive: a stack or manifest with no `[params]`/`{}`/conditions behaves byte-for-byte as today.

---

## File structure

| File | Responsibility |
|---|---|
| `ply-core/src/params.rs` (new) | `ParamDecl`, template engine (`parse_template`, `interpolate`, `refs`), `Namespace` + built-in facts, computed-param resolution, reserved names |
| `ply-core/src/secrets.rs` (new) | `SecretStore` (stack `.ply/secrets/`, deployments `.secrets/`), `load_or_mint`, minting from `/dev/urandom` |
| `ply-core/src/manifest.rs` (modify) | `params` field, reserved-name + hole validation, computed cycle check |
| `ply-core/src/stack.rs` (modify) | `params` member key, `{}`-ref extraction → derived `after` edges, live-param-in-env rejection |
| `ply-core/src/runtime/params_tree.rs` (new) | live file tree under `run_dir()/params/<app>/`, atomic writes, parent-owned list |
| `ply-core/src/runtime/run.rs` (modify) | write live tree on launch/health/stop/restart; mount tree into containers; condition-aware gate |
| `ply-core/src/runtime/after.rs` (modify) | `Wait` grammar, condition polling over param files |
| `ply-core/src/lib.rs` (modify) | `pub mod params; pub mod secrets;` |
| `ply-core/src/runtime/mod.rs` (modify) | `pub mod params_tree;` |
| `ply-cli/src/commands/up.rs` (modify) | resolution pass (namespaces, secrets, taint), `--plan` renderer, bare `-e KEY` spawn for secrets |
| `ply-cli/src/commands/run.rs` (modify) | accept bare `-e KEY` (inherit from process env) |
| `ply-cli/src/commands/secret.rs` (new) | `ply secret ls|set` |
| `ply-cli/src/cli.rs` (modify) | `--plan` on `UpArgs`, `Secret` subcommand |
| `ply-cli/src/commands/mod.rs` (modify) | dispatch `secret` |
| `ply-cli/src/commands/reconcile.rs` (modify) | stack converge uses shared resolution + deployments secret dir |
| `docs/manifest.md`, `docs/stacks.md`, `docs/running.md`, `docs/glossary.md` (modify) | user docs |
| `registry/…/postgres/ply.toml` + redis (modify) | keg `[params]` (owner pushes registry) |

---

### Task 1: Param declarations and the template engine (`params.rs`)

**Files:**
- Create: `ply-core/src/params.rs`
- Modify: `ply-core/src/lib.rs` (add `pub mod params;` alphabetically, after `pub mod oci;`)

**Interfaces:**
- Consumes: `crate::{Error, Result}` (`ply-core/src/error.rs:3-37`).
- Produces:
  - `pub const RESERVED: &[&str]` — the 14 names from Global Constraints
  - `pub const LIVE: &[&str]` — `["state","instances","started_at","restarts"]`
  - `pub enum ParamDecl { Value(String), Secret { external: bool } }` + `impl ParamDecl { pub fn from_toml(name: &str, v: &toml::Value, who: &str) -> Result<ParamDecl> }` — accepts a plain string (default/computed) or `{ secret = true }` / `{ secret = true, external = true }`; anything else is `Error::Manifest` naming `who` and the accepted shapes; `secret = true` with a string value is an error (“minting is the default — remove the value”)
  - `pub struct PRef { pub app: Option<String>, pub param: String }` (`app: None` = own namespace / built-in)
  - `pub enum Piece { Lit(String), Hole(PRef) }`
  - `pub fn parse_template(s: &str, who: &str) -> Result<Vec<Piece>>` — `{{`/`}}` escapes; idents `[A-Za-z0-9_-]+`; one optional dot; any other `{` usage → `Error::Manifest("{who}: stray `{{` in `{s}` — write `{{{{` for a literal brace")`
  - `pub struct Resolved { pub value: String, pub secret: bool }`
  - `pub fn interpolate(pieces: &[Piece], resolve: &mut dyn FnMut(&PRef) -> Result<Resolved>) -> Result<Resolved>` — concatenates; `secret` is OR of all pieces (taint propagation)
  - `pub fn refs(s: &str) -> Vec<PRef>` — best-effort scan (used for edge derivation; returns empty on malformed, real errors surface at interpolation)

- [ ] **Step 1: Write the failing tests**

In `params.rs` bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn lit(pieces: &[Piece]) -> String {
        let mut r = interpolate(pieces, &mut |p: &PRef| {
            Ok(Resolved { value: format!("<{}.{}>", p.app.as_deref().unwrap_or("self"), p.param), secret: p.param == "password" })
        })
        .unwrap();
        std::mem::take(&mut r.value)
    }

    #[test]
    fn plain_text_passes_through() {
        let p = parse_template("hello $VAR world", "t").unwrap();
        assert_eq!(lit(&p), "hello $VAR world");
    }

    #[test]
    fn holes_resolve_and_taint_propagates() {
        let p = parse_template("postgres://{user}:{db.password}@x/{db.database}", "t").unwrap();
        let r = interpolate(&p, &mut |pr: &PRef| Ok(Resolved { value: pr.param.clone(), secret: pr.param == "password" })).unwrap();
        assert_eq!(r.value, "postgres://user:password@x/database");
        assert!(r.secret, "one secret piece taints the whole value");
    }

    #[test]
    fn doubled_braces_are_literals() {
        let p = parse_template("json {{\"a\":1}}", "t").unwrap();
        assert_eq!(lit(&p), "json {\"a\":1}");
    }

    #[test]
    fn a_stray_brace_is_an_error() {
        let e = parse_template("{not valid!}", "t").unwrap_err().to_string();
        assert!(e.contains("stray `{`"), "{e}");
    }

    #[test]
    fn refs_finds_app_scoped_holes() {
        let r = refs("a {db.url} b {port} c {db.host}");
        assert_eq!(r.iter().filter(|p| p.app.as_deref() == Some("db")).count(), 2);
    }

    #[test]
    fn decl_shapes() {
        let v: toml::Value = toml::from_str("x = \"postgres\"").unwrap();
        assert!(matches!(ParamDecl::from_toml("x", &v["x"], "t").unwrap(), ParamDecl::Value(_)));
        let v: toml::Value = toml::from_str("x = { secret = true }").unwrap();
        assert!(matches!(ParamDecl::from_toml("x", &v["x"], "t").unwrap(), ParamDecl::Secret { external: false }));
        let v: toml::Value = toml::from_str("x = { secret = true, default = \"no\" }").unwrap();
        assert!(ParamDecl::from_toml("x", &v["x"], "t").is_err());
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p ply-core params::` — Expected: compile FAIL (module/types missing).

- [ ] **Step 3: Implement**

Tokenizer core (hand-rolled scanner, no regex dep):

```rust
pub fn parse_template(s: &str, who: &str) -> Result<Vec<Piece>> {
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        match c {
            '{' if it.peek().is_some_and(|&(_, n)| n == '{') => { it.next(); lit.push('{'); }
            '}' if it.peek().is_some_and(|&(_, n)| n == '}') => { it.next(); lit.push('}'); }
            '{' => {
                let rest = &s[i + 1..];
                let end = rest.find('}').ok_or_else(|| stray(who, s))?;
                let body = &rest[..end];
                let pref = parse_ref(body).ok_or_else(|| stray(who, s))?;
                for _ in 0..end + 1 { it.next(); }
                if !lit.is_empty() { out.push(Piece::Lit(std::mem::take(&mut lit))); }
                out.push(Piece::Hole(pref));
            }
            _ => lit.push(c),
        }
    }
    if !lit.is_empty() { out.push(Piece::Lit(lit)); }
    Ok(out)
}

fn parse_ref(body: &str) -> Option<PRef> {
    let ident = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    match body.split_once('.') {
        Some((a, p)) if ident(a) && ident(p) => Some(PRef { app: Some(a.into()), param: p.into() }),
        None if ident(body) => Some(PRef { app: None, param: body.into() }),
        _ => None,
    }
}
```

`stray(who, s)` builds the `Error::Manifest` from the interface block. `interpolate` walks pieces, calls `resolve` for holes, ORs `secret`. `refs` calls `parse_template` and filters `Hole`s, returning `Vec::new()` on error.

- [ ] **Step 4: Run tests** — `cargo test -p ply-core params::` — Expected: PASS.
- [ ] **Step 5: Gate** — `make check` — Expected: clean.

---

### Task 2: Manifest `[params]` + validation

**Files:**
- Modify: `ply-core/src/manifest.rs` (struct at `:11-61`, `validate` at `:538-664`)

**Interfaces:**
- Consumes: Task 1 (`ParamDecl`, `parse_template`, `refs`, `RESERVED`, `LIVE`).
- Produces:
  - `Manifest.params: Option<BTreeMap<String, ParamDecl>>` — `None` = table absent (holes NOT interpreted); `Some(empty)` = present. Serde note: `ParamDecl` needs a custom `Deserialize` (string-or-table), or deserialize as `toml::Value` and convert in `validate` — take the second route: field type `Option<BTreeMap<String, toml::Value>>` with `#[serde(default)]`, plus `pub fn param_decls(&self) -> Result<BTreeMap<String, ParamDecl>>` doing conversion + validation. Keep raw values so `Serialize` round-trips for image embedding.
- Validation added to `validate()` (each an `Error::Manifest` with remedy):
  1. Reserved: any `[params]` key in `RESERVED` → `` `{name}` is a built-in param — pick another name ``.
  2. Secret+default handled by `ParamDecl::from_toml` (Task 1).
  3. Computed cycles: build edges param→refs-with-`app: None`; DFS; cycle → `` [params] cycle: a → b → a ``.
  4. Holes: when `params` is `Some`, every `{x}` (no app scope) in `[env]` values and in computed param values must be a declared param or a non-live built-in; `{x.y}` app-scoped refs in a manifest are an error (`a manifest can only reference its own params`); live names → `` `{state}` is live — apps read /run/ply/self/state, dependents wait with `after` ``.
  5. When `params` is `None`, skip all hole checks (braces stay literal).

- [ ] **Step 1: Failing tests** — extend the existing `#[cfg(test)]` in `manifest.rs`, following its TOML-literal style:

```rust
fn manifest_with(params: &str, env: &str) -> String {
    format!("[package]\nname=\"pg\"\nversion=\"1.0.0\"\nentrypoint=[\"pg\"]\n{params}\n[env]\n{env}")
}

#[test]
fn params_reserved_name_rejected() {
    let m = Manifest::parse(&manifest_with("[params]\nhost = \"x\"", "")).unwrap_err().to_string();
    assert!(m.contains("built-in param"), "{m}");
}

#[test]
fn env_hole_must_be_declared_when_params_present() {
    let ok = manifest_with("[params]\nuser = \"postgres\"", "PGUSER = \"{user}\"");
    Manifest::parse(&ok).unwrap();
    let bad = manifest_with("[params]\nuser = \"postgres\"", "PGX = \"{typo}\"");
    assert!(Manifest::parse(&bad).is_err());
}

#[test]
fn braces_are_literal_without_params_table() {
    let m = manifest_with("", "JSONISH = \"{not-a-param!}\"");
    Manifest::parse(&m).unwrap();
}

#[test]
fn computed_cycle_rejected() {
    let m = manifest_with("[params]\na = \"{b}\"\nb = \"{a}\"", "");
    let e = Manifest::parse(&m).unwrap_err().to_string();
    assert!(e.contains("cycle"), "{e}");
}

#[test]
fn live_param_in_env_is_rejected() {
    let m = manifest_with("[params]\nuser = \"x\"", "S = \"{state}\"");
    let e = Manifest::parse(&m).unwrap_err().to_string();
    assert!(e.contains("live"), "{e}");
}
```

- [ ] **Step 2: Run** — `cargo test -p ply-core manifest::` — Expected: FAIL (no field).
- [ ] **Step 3: Implement** — add field after `env` (`manifest.rs:22`) with `#[serde(default, skip_serializing_if = "Option::is_none")]`; add `param_decls()`; wire the five checks into `validate()`. Note `deny_unknown_fields` is already on the struct, so the new field name is load-bearing: `params`.
- [ ] **Step 4: Run** — same filter — Expected: PASS.
- [ ] **Step 5: Gate** — `make check`.

---

### Task 3: Secret store (`secrets.rs`)

**Files:**
- Create: `ply-core/src/secrets.rs`
- Modify: `ply-core/src/lib.rs` (`pub mod secrets;`)

**Interfaces:**
- Consumes: `crate::{Error, Result}`; `std::os::unix::fs::{PermissionsExt, OpenOptionsExt}`.
- Produces:
  - `pub struct SecretStore { dir: PathBuf }`
  - `pub fn for_stack(stack_dir: &Path) -> SecretStore` — `stack_dir/.ply/secrets`
  - `pub fn for_deployments(stack_name: &str) -> SecretStore` — `crate::deployments::dir().join(".secrets").join(stack_name)` (dot-dir: invisible to the systemd `PathModified` watch, same reasoning as `.status/`, `deployments.rs:295-299`)
  - `impl SecretStore`:
    - `pub fn path(&self, member: &str, param: &str) -> PathBuf` — `<dir>/<member>.<param>`
    - `pub fn get(&self, member: &str, param: &str) -> Option<String>` — read + trim trailing newline
    - `pub fn set(&self, member: &str, param: &str, value: &str) -> Result<()>` — create dirs, write `<path>.tmp` with `.mode(0o600)`, rename
    - `pub fn load_or_mint(&self, member: &str, param: &str, external: bool) -> Result<String>` — get, else if `external` → `Error::Runtime(format!("secret {member}.{param} is external — provide it: ply secret set {member}.{param} (or write {})", self.path(member,param).display()))`, else mint+set+return
    - `pub fn list(&self) -> Result<Vec<String>>` — sorted `member.param` names (never values)
  - `pub fn mint() -> String` — read 64 bytes from `/dev/urandom`, map each byte `% 62` into `[A-Za-z0-9]`, take 32 chars (modulo bias is irrelevant at 62/256 for this purpose; URL-safe by construction so `{db.url}` needs no encoding)

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn mint_is_32_urlsafe_chars() {
        let s = mint();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(mint(), s);
    }

    #[test]
    fn load_or_mint_persists_0600_and_is_stable() {
        let td = tempfile::tempdir().unwrap();
        let store = SecretStore::for_stack(td.path());
        let a = store.load_or_mint("db", "password", false).unwrap();
        let b = store.load_or_mint("db", "password", false).unwrap();
        assert_eq!(a, b, "the file is the truth until deleted");
        let mode = std::fs::metadata(store.path("db", "password")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn external_refuses_until_provided() {
        let td = tempfile::tempdir().unwrap();
        let store = SecretStore::for_stack(td.path());
        let e = store.load_or_mint("api", "stripe_key", true).unwrap_err().to_string();
        assert!(e.contains("ply secret set api.stripe_key"), "{e}");
        store.set("api", "stripe_key", "sk_live_x").unwrap();
        assert_eq!(store.load_or_mint("api", "stripe_key", true).unwrap(), "sk_live_x");
    }
}
```

- [ ] **Step 2: Run** — `cargo test -p ply-core secrets::` — FAIL.
- [ ] **Step 3: Implement** per the interface block.
- [ ] **Step 4: Run** — PASS. **Step 5:** `make check`.

---

### Task 4: Namespaces and built-in facts (`params.rs`)

**Files:**
- Modify: `ply-core/src/params.rs`

**Interfaces:**
- Consumes: Tasks 1, 3.
- Produces:
  - `pub struct MemberFacts { pub name: String, pub version: Option<String>, pub host: Option<String>, pub port: Option<u16>, pub scale: u32, pub arch: String, pub image: Option<String> }` — plain data, caller (up.rs, Task 6) fills it: `host = Some(format!("{name}.ply"))` in the stack netns, `port` = container-side port of the first `publish` entry, `version`/`image` from `StackLock` pin or the local manifest, `arch` from the existing target-arch helper used in image naming.
  - `pub fn namespace(facts: &MemberFacts, decls: &BTreeMap<String, ParamDecl>, overrides: &BTreeMap<String, String>, secrets: &mut dyn FnMut(&str) -> Result<String>) -> Result<BTreeMap<String, Resolved>>`
    - facts first (never secret): `name`, `scale`, `arch`, plus `version`/`image` when known; `host`/`port` when present; `addr = "{host}:{port}"` and `base_url = "http://{host}:{port}"` only when both exist.
    - declared params next: override value wins over declared default (overrides come `$VAR`-pre-expanded from stack code — Task 5); `Secret{..}` → `secrets(param)` closure (up wires `SecretStore::load_or_mint`; an override supplants minting), always `secret: true`.
    - computed params (values containing holes) resolved recursively against the namespace itself; unresolvable ref → `Error::Manifest("{name}: `{{{param}}}` is not a param of {name} — declared: {list}")`; missing `host`/`port` → `"{name}: `{{port}}` — {name} publishes no port"`.
  - `pub fn lookup<'a>(ns: &'a BTreeMap<String, Resolved>, r: &PRef, ...) -> Result<&'a Resolved>` helper that rejects LIVE names with the live-param error (reused by stack + up).

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn namespace_builds_facts_and_computed_url() {
    let facts = MemberFacts { name: "db".into(), version: Some("17.10.3".into()),
        host: Some("db.ply".into()), port: Some(5432), scale: 1, arch: "x64".into(), image: None };
    let mut decls = BTreeMap::new();
    decls.insert("user".into(), ParamDecl::Value("postgres".into()));
    decls.insert("database".into(), ParamDecl::Value("postgres".into()));
    decls.insert("password".into(), ParamDecl::Secret { external: false });
    decls.insert("url".into(), ParamDecl::Value("postgres://{user}:{password}@{host}:{port}/{database}".into()));
    let mut overrides = BTreeMap::new();
    overrides.insert("database".into(), "todos".into());
    let ns = namespace(&facts, &decls, &overrides, &mut |_| Ok("S3CR3T".into())).unwrap();
    assert_eq!(ns["base_url"].value, "http://db.ply:5432");
    assert_eq!(ns["url"].value, "postgres://postgres:S3CR3T@db.ply:5432/todos");
    assert!(ns["url"].secret && !ns["database"].secret, "taint follows the password into url");
}

#[test]
fn port_ref_without_publish_names_the_gap() {
    let facts = MemberFacts { name: "job".into(), version: None, host: None, port: None, scale: 1, arch: "x64".into(), image: None };
    let mut decls = BTreeMap::new();
    decls.insert("url".into(), ParamDecl::Value("http://{host}:{port}".into()));
    let e = namespace(&facts, &decls, &BTreeMap::new(), &mut |_| unreachable!()).unwrap_err().to_string();
    assert!(e.contains("publishes no port"), "{e}");
}
```

- [ ] **Step 2: Run** — FAIL. **Step 3: Implement** (recursive resolution reuses `parse_template` + `interpolate`; manifest cycle validation from Task 2 already guarantees termination for image-borne decls — still guard with a depth counter and the cycle error for stack-supplied overrides). **Step 4: Run** — PASS. **Step 5:** `make check`.

---

### Task 5: Stack surface — `params` key, derived edges, validation

**Files:**
- Modify: `ply-core/src/stack.rs` (`Member` `:38-56`, `MEMBER_KEYS` `:410-412`, `parse_member` `:432-509`, overlay structs `:195-269`)

**Interfaces:**
- Consumes: Task 1 (`refs`, `PRef`, `LIVE`).
- Produces:
  - `Member.params: Vec<(String, String)>` — raw, `$VAR` unexpanded (same treatment as `env`); parsed from a `params` inline table or table of strings; unknown-key handling comes free from `MEMBER_KEYS` (append `"params"`).
  - `pub fn expand_member_params(member: &Member, lookup: &impl Fn(&str) -> Option<String>) -> Result<BTreeMap<String, String>>` — mirrors `expand_member_env` (`:716-729`), so `params = { password = "$PROD_PW" }` fills from env-file/environment.
  - `pub fn derived_after(member: &Member) -> Vec<String>` — scan every `env` value and `publish`/`domain` entry with `params::refs`, collect distinct `app` names ≠ own name; **union into the ordering**: modify the member-ordering/cycle code (exercised by `parses_and_orders_the_lab_stack` `:956` and `rejects_cycles` `:1041`) to consider `after ∪ derived_after`, and error on a ref to an unknown member: `"{member}: `{{{app}.{param}}}` references no stack member named `{app}`"`.
  - Live rejection at parse: any env value ref with `param` in `LIVE` → `Error::Manifest("{member}: `{{{app}.{p}}}` is live — wait on it with `after = [\"{app}.{p} == '…'\"]`, or read /run/ply/{app}/{p} at runtime")`.
  - Overlay: `MemberOverlay` gains `params` (merge **by key**, like env `:157-169`).

- [ ] **Step 1: Failing tests** — add to `mod tests` (helper `stack_of` `:921` style):

```rust
#[test]
fn member_params_parse_and_stay_raw() {
    let s = stack_of(r#"
[[app]]
run = "postgres@17"
name = "db"
params = { database = "todos", password = "$PW" }
"#);
    let m = &s.members[0];
    assert!(m.params.contains(&("password".into(), "$PW".into())));
    let x = expand_member_params(m, &|k| (k == "PW").then(|| "s".into())).unwrap();
    assert_eq!(x["password"], "s");
}

#[test]
fn env_refs_derive_the_edge_and_order() {
    let s = stack_of(r#"
[[app]]
run = "./server"
e = ["DATABASE_URL={db.url}"]
[[app]]
run = "postgres@17"
name = "db"
"#);
    assert_eq!(s.members.last().unwrap().name, "server", "db ordered first via the ref");
}

#[test]
fn ref_to_unknown_member_is_an_error() {
    let e = stack_err(r#"
[[app]]
run = "./server"
e = ["X={ghost.url}"]
"#);
    assert!(e.contains("no stack member named `ghost`"), "{e}");
}

#[test]
fn live_param_in_env_is_rejected_with_the_wait_hint() {
    let e = stack_err(r#"
[[app]]
run = "./server"
e = ["S={db.state}"]
[[app]]
run = "postgres@17"
name = "db"
"#);
    assert!(e.contains("is live"), "{e}");
}
```

(`stack_err` = tiny helper: parse, unwrap_err, to_string — add beside `stack_of`.)

- [ ] **Step 2: Run** — `cargo test -p ply-core stack::` — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS, including all 28 pre-existing stack tests. **Step 5:** `make check`.

---

### Task 6: `ply up` resolution pass + secret-safe spawn

**Files:**
- Modify: `ply-cli/src/commands/up.rs` (`Prepared` `:33-48`, `exec` `:50-210`, spawn `:145-182`, `prepare_target` `:284-353`)
- Modify: `ply-cli/src/commands/run.rs:44-49` (bare `-e KEY`)
- Modify: `ply-core/src/stack.rs` (one helper, below)

**Interfaces:**
- Consumes: Tasks 1–5; `StackLock` (`stack.rs:822-913`); embedded-manifest read — the same `/.manifest.toml` extraction `prepare_app` relies on (`ply-core/src/runtime/run.rs:942-1038`; the reader lives with the image/store code — grep `manifest.toml` under `ply-core/src/image/` and expose it `pub` if it isn't).
- Produces:
  - In `stack.rs`: `pub fn member_manifest(target: &str, dir: Option<&Path>, store: &Store) -> Result<Manifest>` — Path member → `Manifest::load(dir/ply.toml)`; image target → embedded read.
  - In `up.rs`:
    - `struct ResolvedEnv { key: String, value: String, secret: bool, source: EnvSource }`
    - `enum EnvSource { StackE, ParamRef(String), SelfEnv, Minted, Override, Ambient }` (`Display` for `--plan`, Task 9)
    - `fn resolve_members(prepared: &mut [Prepared], stack_dir: Option<&Path>, store: &Store, plan_only: bool) -> Result<Resolution>` where `Resolution { namespaces: BTreeMap<String, BTreeMap<String, Resolved>>, env: BTreeMap<String, Vec<ResolvedEnv>>, waits: BTreeMap<String, Vec<String>> }`:
      1. per member: `member_manifest` → `param_decls()`; `MemberFacts` from name/pin/publish (container port = part after last `:` of the first publish entry, same value `discovery_env`'s netns branch uses, `run.rs:1550-1553`); overrides via `expand_member_params`.
      2. secrets closure: `SecretStore::for_stack(stack_dir)` `load_or_mint`; when `plan_only`, missing file resolves to the literal `(will mint)` with `secret: true` and **no write**.
      3. `namespace(...)` per member; then each member's env values: existing `$` expansion first (unchanged, `:65-68`), then `parse_template` + `interpolate` with cross-member `lookup` (Task 4) — LIVE names already rejected at parse, so this is belt-and-braces.
      4. provider self-config: if the member's manifest has `params` and hole-y `[env]` values, resolve each against its own namespace and emit as `SelfEnv` entries — these are passed as `-e` overrides, which win over the manifest's literal value inside the child (compose order, `env.rs:34-39`; the child never sees an unresolved `{password}` because the override shadows it).
      5. `waits` = `after ∪ derived_after`.
  - Spawn changes (`:145-182`): for each `ResolvedEnv` — `secret: false` → `-e KEY=VALUE` as today; `secret: true` → `.env(&key, &value)` on the `Command` + bare `-e KEY` (value stays out of argv and `/proc/*/cmdline`).
  - `run.rs:44-49` grows the bare form:

    ```rust
    for pair in &args.env {
        match pair.split_once('=') {
            Some((k, v)) => cli_env.push((k.to_string(), v.to_string())),
            None => match std::env::var(pair) {
                Ok(v) => cli_env.push((pair.clone(), v)),
                Err(_) => anyhow::bail!("-e {pair}: not set in the environment (bare -e KEY inherits the caller's value)"),
            },
        }
    }
    ```

- [ ] **Step 1: Failing tests** — `resolve_members` is testable without spawning: build `Prepared` fixtures with a tempdir Path member (write a minimal `ply.toml` with `[params]`) and assert url/taint/waits; bare `-e` gets a unit test in `run.rs`'s existing test module (`env parsing` area) asserting both arms. Write them first, in `up.rs`'s (new) `#[cfg(test)] mod tests` and `run.rs`'s existing one.
- [ ] **Step 2: Run** — `cargo test -p ply-cli` — FAIL.
- [ ] **Step 3: Implement** per the interface block. Keep `exec()`'s existing flow; the resolution pass slots in after `:85-87` (publish/domain expansion) and before spawn.
- [ ] **Step 4: Run** — PASS. **Step 5:** `make check`.
- [ ] **Step 6: Manual smoke (build only, no droplet):** in `/home/iluxa/projects/ply-labs`, temporarily add `params = { database = "todos" }` to the db member and `e = ["DATABASE_URL={db.url}"]` to server in a scratch copy of `stack.toml` — `cargo run -p ply-cli -- up -C /tmp/…scratch --plan` once Task 9 lands; until then verify via the unit tests only. (Runtime verification happens on the droplet per project rule — never a backgrounded `ply run` on this box.)

---

### Task 7: Live params tree (`runtime/params_tree.rs`) + container mount

**Files:**
- Create: `ply-core/src/runtime/params_tree.rs`
- Modify: `ply-core/src/runtime/mod.rs` (`pub mod params_tree;`)
- Modify: `ply-core/src/runtime/run.rs` — write sites: instance-state save (`:1424-1455`), roll health gate pass/fail (`:757-766`), stop path, restart accounting; mount site: alongside the per-instance `/etc/hosts` bind (pattern: `hosts.rs:129`, instance dirs `run.rs:2109-2120`)

**Interfaces:**
- Consumes: `crate::paths::run_dir()` (`paths.rs:33-41`).
- Produces:
  - `pub const PARENT_OWNED: &[&str] = &["state", "instances", "started_at", "restarts"];`
  - `pub fn dir(app: &str) -> PathBuf` — `run_dir()/params/<app>`
  - `pub fn publish(app: &str, key: &str, value: &str) -> Result<()>` — create dir, tmp-write + rename (atomic, same idiom as `deployments::write_status` `:310-325`)
  - `pub fn read(app: &str, key: &str) -> Option<String>` — read + trim
  - `pub fn remove_app(app: &str)` — best-effort cleanup on final stop
- Write points (parent side, host view):
  - at instance launch (with `InstanceState::save`, `:1455`): `state=starting`, `started_at=<epoch>`, `instances=<n>`, `restarts=0`, plus facts `name`, `version`, `host`, `port` when known — **never a secret** (facts only; declared param values do not enter the tree in v1 beyond what the parent already knows)
  - health gate pass (`wait_healthy` true path / after first successful probe): `state=healthy`; gate fail: `state=unhealthy`
  - restart loop: increment `restarts`; stop: `state=stopped`
- Container view: bind `run_dir()/params` → `/run/ply` **read-only**; then bind the app's own `dir(app)` → `/run/ply/self` **read-write**; then re-bind each `PARENT_OWNED` file inside `/run/ply/self` read-only over itself (parent pre-creates all four before mounting, so the ro binds always have a target). Result: an app writes `/run/ply/self/finish_boot`, cannot forge `state`, and reads any neighbor. Instances of one app share the node — last-writer-wins is accepted (spec).

- [ ] **Step 1: Failing tests** (tree logic only — mounts are verified on the droplet):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // run_dir() honors $XDG_RUNTIME_DIR rootless (paths.rs:33-41) — point it at a tempdir.
    #[test]
    fn publish_read_roundtrip_and_atomicity() {
        let td = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", td.path());
        publish("db", "state", "healthy").unwrap();
        assert_eq!(read("db", "state").as_deref(), Some("healthy"));
        publish("db", "state", "stopped").unwrap();
        assert_eq!(read("db", "state").as_deref(), Some("stopped"));
        assert!(read("db", "ghost").is_none());
    }
}
```

(If other tests race on env vars, reuse the `ENV_LOCK` mutex pattern from `e2e_resolve.rs:13`.)

- [ ] **Step 2: Run** — FAIL. **Step 3: Implement** module + write points + mounts (mount code goes where the hosts bind is established for each instance; keep it inside the existing mount sequence so ordering/cleanup is inherited). **Step 4: Run** — PASS. **Step 5:** `make check`.
- [ ] **Step 6: Record for droplet verification** — add to `TASKS.md` verification notes: `ply run` an app on the test droplet, `cat /run/ply/<app>/state` from a neighbor `ply exec`, confirm `echo x > /run/ply/<peer>/state` fails (ro) and `echo ok > /run/ply/self/finish_boot` succeeds. (Owner runs these; local box can't run ply — AppArmor.)

---

### Task 8: `after` conditions

**Files:**
- Modify: `ply-core/src/runtime/after.rs` (`Readiness` `:16-21`, `check` `:56-64`, `wait_until` `:68-114`, `wait_for` `:117-121`, tests `:194`)
- Modify: `ply-core/src/runtime/run.rs:293-315` (gate) and `:1537-1567` (`discovery_env` — dedupe on app names from parsed waits)
- Modify: `ply-cli/src/cli.rs:326-333` (`--after` help text: document the three forms)

**Interfaces:**
- Consumes: Task 7 (`params_tree::read`), Task 1 (ident rules).
- Produces:
  - `pub struct Wait { pub app: String, pub param: Option<String>, pub equals: Option<String> }`
  - `pub fn parse_wait(s: &str) -> Result<Wait>` — three forms exactly; `app.param == 'lit'` / `== "lit"` (whitespace around `==` optional); anything else → `Error::Runtime("--after `{s}`: expected APP, APP.PARAM, or APP.PARAM == 'value'")`. `!=`, `>`, `&&` → same error (grammar is closed).
  - `pub fn check_wait(w: &Wait) -> Readiness` — `param: None` → existing `check(app)` (bare form unchanged: alive + port probe); `Some(p)` → `params_tree::read(app, p)`: `None` → `Readiness::Unhealthy(format!("{}.{} (currently unset)", w.app, p))`; `Some(v)` with `equals: Some(e)` and `v != e` → `Unhealthy(format!("{}.{} == '{}' (currently '{}')", w.app, p, e, v))`; otherwise `Ready`.
  - `wait_for` takes `&[Wait]`; timeout error message carries condition + current + elapsed (the `wait_until` `report` closure already streams progress; final error format per Global Constraints).
  - Gate at `run.rs:301-314`: parse `opts.after` into `Vec<Wait>` up front (parse errors fail fast, before `WaitingMarker`); `WaitingMarker` and `discovery_env` receive the **distinct app names**.

- [ ] **Step 1: Failing tests** (after.rs `mod tests` — `wait_until` already accepts an injected `poll` closure, use it):

```rust
#[test]
fn wait_grammar_three_forms_only() {
    assert_eq!(parse_wait("db").unwrap().app, "db");
    let w = parse_wait("server.finish_boot == 'ok'").unwrap();
    assert_eq!((w.param.as_deref(), w.equals.as_deref()), (Some("finish_boot"), Some("ok")));
    assert!(parse_wait("server.finish_boot != 'ok'").is_err());
    assert!(parse_wait("a.b == 'x' && c").is_err());
}

#[test]
fn condition_waits_until_value_matches() {
    let seq = std::cell::RefCell::new(vec![None, Some("booting".to_string()), Some("ok".to_string())]);
    // drive check_wait via a poll closure that reads from seq — Ready only on "ok"
    // assert wait_until returns Ok after three polls; assert the intermediate report
    // strings contain "currently unset" then "currently 'booting'".
    /* full closure-based test in the style of after.rs's existing mod tests */
}
```

(The second test's body follows the existing `wait_until` test idiom at `after.rs:194` — poll closure + collected report strings; write it out fully, asserting both intermediate messages.)

- [ ] **Step 2: Run** — `cargo test -p ply-core after::` — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS. **Step 5:** `make check`.
- [ ] **Step 6:** Extend the Task 7 droplet-verification note: two-member stack where the dependent uses `after = ["a.finish_boot == 'ok'"]` and member `a` writes the file after a sleep — dependent must hold, then start; kill the write → dependent aborts at timeout with the exact message shape.

---

### Task 9: `ply up --plan`

**Files:**
- Modify: `ply-cli/src/cli.rs:367-395` (`UpArgs` + `#[arg(long)] plan: bool`)
- Modify: `ply-cli/src/commands/up.rs`

**Interfaces:**
- Consumes: Task 6 (`Resolution`, `EnvSource`), Task 5 (`derived_after`).
- Produces: `fn render_plan(resolution: &Resolution, prepared: &[Prepared]) -> String` — per member, in launch order:
  - header: `name (target)  publish  after: x (via {x.url}), y` — derived edges annotated `via`, explicit ones bare
  - one line per env entry: `KEY = VALUE  source` — `Display` of `EnvSource`; `secret: true` values render `********` **inside composed strings too** (mask the secret substring, e.g. `postgres://postgres:********@db.ply:5432/todos`; a minted-not-yet value renders `(will mint)`)
  - `exec()` with `--plan`: run everything up to and including `resolve_members(plan_only: true)` — no downloads beyond what target prep already did, **no minting, no spawn, no lock write** — print, exit 0; any resolution error prints and exits non-zero (plan is the validator).

- [ ] **Step 1: Failing test** — unit-test `render_plan` on a hand-built `Resolution` (no I/O): assert the masked url line, the `(will mint)` line, the `via {db.url}` edge annotation, and that a `secret: false` value prints verbatim.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS. **Step 5:** `make check`.
- [ ] **Step 6: Manual:** `cargo run -p ply-cli -- up -C /home/iluxa/projects/ply-labs --plan` (safe on this box — plan spawns nothing). Expected: three members, todos-shaped output matching the spec's sample; password masked everywhere.

---

### Task 10: `ply secret ls|set`

**Files:**
- Create: `ply-cli/src/commands/secret.rs`
- Modify: `ply-cli/src/cli.rs` (subcommand enum + `SecretArgs`), `ply-cli/src/commands/mod.rs` (dispatch, `:34-35` area)

**Interfaces:**
- Consumes: Task 3 (`SecretStore`).
- Produces:
  - `ply secret ls [-C DIR]` — lists names from `SecretStore::for_stack(dir)` (default `.`), one per line, values never shown; empty store prints `no secrets under <path>` to stderr, exit 0.
  - `ply secret set NAME [VALUE] [-C DIR]` — `NAME` = `member.param` (validate exactly one dot); `VALUE` omitted → read one trimmed line from stdin (so values stay out of shell history); confirms `wrote <path> (0600)`.
  - Help text notes the deployments-side store is plain files under `/var/lib/ply/deployments/.secrets/<stack>/` — ops can manage those with the same command via `-C`… no: `for_deployments` is keyed by name, so add `--deployments STACK` as the alternative selector (mutually exclusive with `-C`).

- [ ] **Step 1: Failing tests** — in `secret.rs` `#[cfg(test)]`: name validation (`db.password` ok, `db` and `a.b.c` rejected with the format hint), and an ls/set round-trip against `SecretStore::for_stack(tempdir)` through the command fns (factor `fn ls(store) -> Vec<String>` / `fn set(store, name, value) -> Result<PathBuf>` so tests skip clap).
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS. **Step 5:** `make check`.

---

### Task 11: Deployments/reconcile path

**Files:**
- Modify: `ply-cli/src/commands/reconcile.rs` (`converge_stack` `:341-…`, lookup `:365`, secret-file resolution helper `resolve_secret` `:724-733`)
- Modify: `ply-core/src/deployments.rs` (`Spec::from_stack_member` `:250-299`) — only if signature must carry resolved values; prefer resolving before the call

**Interfaces:**
- Consumes: Task 6's `resolve_members` — **factor its core** (`fn resolve_stack(stack: &Stack, lock: …, secrets: &mut SecretStore, lookup: …, plan_only: bool) -> Result<Resolution>`) out of `up.rs` into `ply-core/src/stack.rs` during this task, so up and reconcile share one resolver; up.rs keeps only Prepared-glue.
- Produces: a reconciled stack deployment resolves `{}` refs identically to `ply up`, with `SecretStore::for_deployments(stack_name)`. Secret-tainted values go into the per-app `.env/<name>.env` file the reconciler already binds (`reconcile.rs:98-107`) rather than unit `-e` flags — units are world-readable under `/etc/systemd/system` (`:12`), env files are not.
- Ordering note: reconcile's converge must apply `after ∪ derived_after` when it emits `--after` flags for members (reuse `derived_after` — waits list from `Resolution`).

- [ ] **Step 1: Failing test** — core-side: move/factor test from Task 6 to cover `resolve_stack` directly (stack TOML literal + tempdir secrets + fake lookup); reconcile-side: unit-test the env-file-vs-flags split decision fn (`fn split_env(entries: &[ResolvedEnv]) -> (Vec<(String,String)> /*flags*/, Vec<(String,String)> /*file*/)` — secret→file, plain→flags).
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS. **Step 5:** `make check`.
- [ ] **Step 6:** Append droplet verification to `TASKS.md`: deploy the todos stack as a deployment file with `params`, confirm minted file at `/var/lib/ply/deployments/.secrets/todos/db.password`, confirm the unit file contains no secret, confirm app connects.

---

### Task 12: Docs + kegs

**Files:**
- Modify: `docs/manifest.md` (new `[params]` section after `[env]` `:117-118`; hole rules; reserved list)
- Modify: `docs/stacks.md` (rewrite the env section around `{app.param}`; keep `$VAR` docs; add derived edges + wait grammar + `--plan`; mark `env_file` "superseded for secrets")
- Modify: `docs/running.md` (`Environment` section `:307-317`: bare `-e KEY`; deprecate `--after` injection in favor of refs — keep behavior documented; add `/run/ply` tree + self-publish + wait conditions)
- Modify: `docs/glossary.md` (param, fact, live param, minted secret, taint)
- Modify: postgres + redis keg manifests under the registry tree (`[params]` + `[env]` holes as in the spec's postgres example; `redis`: `password = { secret = true }` optional? — redis auth is opt-in: ship `url = "redis://{host}:{port}"` only, no secret, v1)
- Modify: `TASKS.md` — add the phase entry with the droplet-verification checklist accumulated in Tasks 7/8/11

**Steps:**

- [ ] **Step 1:** Write the four doc updates; every example must be copy-paste runnable against the todos stack; the stacks.md example IS the spec's final stack.toml (three members, `params = { database = "todos" }`, `DATABASE_URL={db.url}`).
- [ ] **Step 2:** Update the two keg manifests; run `cargo run -p ply-cli -- check` against a locally built postgres keg image if the keg build scripts allow, else validate with `Manifest::parse` via `cargo test`.
- [ ] **Step 3:** `make check` still green (docs don't compile, but keg TOML fixtures might be referenced by tests — confirm).
- [ ] **Step 4:** Hand the owner the registry-push list: postgres, redis keg updates (`registry-push` per production-deploy procedure — owner runs it; live registry).

---

## Self-review (done)

- **Spec coverage:** `[params]` decls/computed/secret/external → T1-T3; `[env]` holes/self-config → T2/T6; built-ins incl. `base_url` → T4; `{app.param}` + `$` coexistence, escapes, no-expression-language → T1/T5/T6; stack `params` override with `$VAR` → T5; derived edges → T5/T6/T11; live-in-env hard error → T2/T5; `/run/ply` tree + self-publish + ro parent files + no secrets in tree → T7; wait grammar `==`-only + loud timeout → T8; minted 0600 files + external refuse + `ply secret` → T3/T10; taint masking → T1/T9; `--plan` attribution + validator exit → T9; deployments/.secrets + env-file delivery → T11; docs/kegs/migration-additivity → T12 + Global Constraints. **Deviations from spec, deliberate:** `image` fact is the resolved target string in v1 (digest only when pinned — noted in T4); masking scope v1 is plan/up output (`ply ps` shows no env today; app-emitted logs can't be masked); `ply.dev.toml`-level `{}` holes not added (dev overlay merges by key before resolution, so refs in overlays flow through T6 untouched — they work, but are not separately tested; acceptable).
- **Placeholder scan:** Task 8 Step 1 second test body is deliberately outlined with exact assertions and points at the existing idiom at `after.rs:194` — executor writes the closure; all other steps carry real code or exact anchors. No TBDs.
- **Type consistency:** `Resolved{value,secret}` (T1) used by T4/T5/T6/T9/T11; `ResolvedEnv`/`EnvSource`/`Resolution` defined T6, consumed T9/T11 (T11 moves the resolver core to `stack.rs` — names unchanged); `Wait` defined T8 and consumed by run.rs gate; `SecretStore` (T3) consumed T6/T10/T11; `params_tree::{publish,read,PARENT_OWNED}` (T7) consumed T8. Consistent.
