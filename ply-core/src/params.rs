//! Parameter declarations and template engine.
//!
//! A param is a named value that can be interpolated into templates using
//! `{name}` for own-namespace (built-in) params or `{app.name}` for app-scoped
//! ones. Params can be plain values or secrets (minted or external).

use std::collections::BTreeMap;

use crate::{Error, Result};

/// The 14 reserved names that cannot be used as custom param names.
pub const RESERVED: &[&str] = &[
    "name",
    "version",
    "host",
    "port",
    "addr",
    "base_url",
    "scale",
    "arch",
    "image",
    "state",
    "instances",
    "started_at",
    "restarts",
    "self",
];

/// Live params that are populated by the runtime, never user-declared.
pub const LIVE: &[&str] = &["state", "instances", "started_at", "restarts"];

/// A parameter declaration from a manifest.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamDecl {
    /// A plain value (default/computed).
    Value(String),
    /// A secret value (minted or external).
    Secret { external: bool },
}

impl ParamDecl {
    /// Parse a param declaration from a TOML value.
    ///
    /// Accepts:
    /// - A plain string: becomes `Value(string)`
    /// - `{ secret = true }`: becomes `Secret { external: false }`
    /// - `{ secret = true, external = true }`: becomes `Secret { external: true }`
    ///
    /// Rejects:
    /// - `{ secret = true }` with a string value ("minting is the default — remove the value")
    /// - Any other shape
    pub fn from_toml(name: &str, v: &toml::Value, who: &str) -> Result<ParamDecl> {
        match v {
            toml::Value::String(s) => Ok(ParamDecl::Value(s.clone())),
            toml::Value::Table(t) => {
                let secret = t.get("secret").and_then(|v| v.as_bool()).unwrap_or(false);
                let external = t.get("external").and_then(|v| v.as_bool()).unwrap_or(false);
                let expected_keys = (if t.contains_key("secret") { 1 } else { 0 })
                    + (if t.contains_key("external") { 1 } else { 0 });
                let has_default = t.len() > expected_keys;

                if !secret {
                    return Err(Error::Manifest(format!(
                        "{who}: param `{name}` must be a string or {{ secret = true }} — got {v:?}",
                    )));
                }

                if has_default {
                    return Err(Error::Manifest(format!(
                        "{who}: param `{name}` is secret but has a default value — minting is the default, remove the value",
                    )));
                }

                Ok(ParamDecl::Secret { external })
            }
            _ => Err(Error::Manifest(format!(
                "{who}: param `{name}` must be a string or {{ secret = true }} — got {v:?}",
            ))),
        }
    }
}

/// A parameter reference: `{param}` or `{app.param}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PRef {
    /// App namespace (None = own namespace / built-in).
    pub app: Option<String>,
    /// Parameter name.
    pub param: String,
}

/// A fragment of an interpolated template.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    /// Literal text.
    Lit(String),
    /// A hole to be filled by interpolation.
    Hole(PRef),
}

/// Parse a template string into pieces.
///
/// Syntax:
/// - `{{` and `}}` are literal braces.
/// - `{ident}` is a param reference.
/// - `{ident.ident}` is an app-scoped param reference.
/// - Any other `{` is an error.
pub fn parse_template(s: &str, who: &str) -> Result<Vec<Piece>> {
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut it = s.char_indices().peekable();

    while let Some((i, c)) = it.next() {
        match c {
            '{' if it.peek().is_some_and(|&(_, n)| n == '{') => {
                it.next();
                lit.push('{');
            }
            '}' if it.peek().is_some_and(|&(_, n)| n == '}') => {
                it.next();
                lit.push('}');
            }
            '{' => {
                let rest = &s[i + 1..];
                let end = rest.find('}').ok_or_else(|| stray(who, s))?;
                let body = &rest[..end];
                let pref = parse_ref(body).ok_or_else(|| stray(who, s))?;
                // Skip past the hole body and closing brace
                for _ in 0..=end {
                    it.next();
                }
                if !lit.is_empty() {
                    out.push(Piece::Lit(std::mem::take(&mut lit)));
                }
                out.push(Piece::Hole(pref));
            }
            _ => lit.push(c),
        }
    }

    if !lit.is_empty() {
        out.push(Piece::Lit(lit));
    }

    Ok(out)
}

/// Parse a reference body (the part between braces).
fn parse_ref(body: &str) -> Option<PRef> {
    let ident = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };

    match body.split_once('.') {
        Some((a, p)) if ident(a) && ident(p) => Some(PRef {
            app: Some(a.into()),
            param: p.into(),
        }),
        None if ident(body) => Some(PRef {
            app: None,
            param: body.into(),
        }),
        _ => None,
    }
}

/// Build a stray brace error.
fn stray(who: &str, s: &str) -> Error {
    Error::Manifest(format!(
        "{who}: stray `{{` in `{s}` — write `{{{{` for a literal brace"
    ))
}

/// A resolved parameter value.
#[derive(Clone, PartialEq)]
pub struct Resolved {
    /// The resolved value.
    pub value: String,
    /// Whether this value (or any of its pieces) is secret.
    pub secret: bool,
}

/// Hand-written (not derived): a secret's `value` must never come out of a
/// `{:?}` — a stray `dbg!`, or an `anyhow` context on a type that embeds
/// this (a whole `Resolution`, up two crates, included) — the way a derive
/// would print it verbatim.
impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value: &dyn std::fmt::Debug = if self.secret {
            &"********"
        } else {
            &self.value
        };
        f.debug_struct("Resolved")
            .field("value", value)
            .field("secret", &self.secret)
            .finish()
    }
}

/// Interpolate pieces by resolving each hole.
///
/// The `resolve` callback is called for each hole. If any piece is secret,
/// the result is marked secret (taint propagation).
pub fn interpolate(
    pieces: &[Piece],
    resolve: &mut dyn FnMut(&PRef) -> Result<Resolved>,
) -> Result<Resolved> {
    let mut value = String::new();
    let mut secret = false;

    for piece in pieces {
        match piece {
            Piece::Lit(s) => value.push_str(s),
            Piece::Hole(pref) => {
                let resolved = resolve(pref)?;
                value.push_str(&resolved.value);
                secret = secret || resolved.secret;
            }
        }
    }

    Ok(Resolved { value, secret })
}

/// Best-effort scan for parameter references in a string.
///
/// Used for edge derivation. Returns empty on malformed templates;
/// real errors surface at interpolation.
pub fn refs(s: &str) -> Vec<PRef> {
    parse_template(s, "")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| match p {
            Piece::Hole(pref) => Some(pref),
            _ => None,
        })
        .collect()
}

/// Plain facts about a stack member, supplied by the caller (`ply up` /
/// stack composition). Never secret.
///
/// `host`/`port` are populated only when the member has a netns identity and
/// publishes a port respectively; `addr`/`base_url` are derived by
/// [`namespace`] only when both are present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberFacts {
    pub name: String,
    pub version: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub scale: u32,
    pub arch: String,
    pub image: Option<String>,
}

/// Build the "no such param" error, special-casing the four address/port
/// built-ins — those are absent because of the run's shape, not because of a
/// typo — vs. any other unresolvable name.
///
/// The two shapes are different gaps with different remedies, so they read
/// differently: `{port}` (and `{addr}`/`{base_url}` when the address itself
/// IS available) is about publishing a port; `{host}` (and `{addr}`/
/// `{base_url}` behind it) is about a run with no `<name>.ply` address at
/// all — a bare rootless `ply run`, where the remedy is root or a stack, not
/// a `--publish`. `host_known` says which of the two the caller is looking
/// at.
fn no_such_param<'a>(
    who: &str,
    name: &str,
    host_known: bool,
    declared: impl Iterator<Item = &'a str>,
) -> Error {
    if matches!(name, "host" | "addr" | "base_url") && !host_known {
        return Error::Manifest(format!(
            "{who}: `{{{name}}}` — this run has no `{who}.ply` address; run it rootful or inside a stack"
        ));
    }
    if matches!(name, "host" | "port" | "addr" | "base_url") {
        return Error::Manifest(format!("{who}: `{{{name}}}` — {who} publishes no port"));
    }
    let list: Vec<&str> = declared.collect();
    Error::Manifest(format!(
        "{who}: `{{{name}}}` is not a param of {who} — declared: {}",
        list.join(", ")
    ))
}

/// Build the "live param" error: `state`/`instances`/`started_at`/`restarts`
/// are populated by the runtime, never resolvable statically.
fn live_error(who: &str, name: &str) -> Error {
    Error::Manifest(format!(
        "{who}: `{{{who}.{name}}}` is live — wait on it with \
         `after = [\"{who}.{name} == '…'\"]`, or read /run/ply/{who}/{name} at runtime"
    ))
}

/// One member's resolved param namespace: every built-in fact this run has
/// and every declared param, each either resolved or holding the message
/// explaining why it could not be.
///
/// A param that cannot resolve is NOT an error until something reads it: a
/// keg's computed `url = "…{host}:{port}…"` is dead weight on a run that
/// publishes nothing, and must not stop the rest of the namespace (and so
/// the whole `ply run` / `ply up`) from working. [`namespace`] captures such
/// a failure here; [`lookup`] turns it back into the same error it always
/// was, at the point of reference. The `Err` string is a message only —
/// never a value, secret or otherwise.
pub type Namespace = BTreeMap<String, std::result::Result<Resolved, String>>;

/// Look up a param reference in a resolved namespace, rejecting live
/// (runtime-only) names and surfacing a captured resolution failure.
///
/// `ns` is one member's already-[`namespace`]-resolved params; `who` names
/// the member `ns` belongs to (used to build the same error shapes
/// `namespace` itself produces). Shared by stack composition and `ply up`'s
/// cross-member `{app.param}` resolution.
pub fn lookup<'a>(ns: &'a Namespace, r: &PRef, who: &str) -> Result<&'a Resolved> {
    let name = r.param.as_str();
    if LIVE.contains(&name) {
        return Err(live_error(who, name));
    }
    match ns.get(name) {
        Some(Ok(resolved)) => Ok(resolved),
        // The gap this param hit when the namespace was built, named now
        // that something has actually read it — verbatim, so a reference
        // reports exactly what an eager resolution used to.
        Some(Err(msg)) => Err(Error::Manifest(msg.clone())),
        None => Err(no_such_param(
            who,
            name,
            ns.contains_key("host"),
            ns.keys().map(String::as_str),
        )),
    }
}

/// Resolution context threaded through the recursive descent in
/// [`Ctx::resolve`]. `facts`/`decls`/`overrides` are borrowed for the whole
/// pass (references are `Copy`, so holding them as fields never conflicts
/// with the `&mut self` mutation of `ns`/`resolving`).
struct Ctx<'a> {
    facts: &'a MemberFacts,
    decls: &'a BTreeMap<String, ParamDecl>,
    overrides: &'a BTreeMap<String, String>,
    secrets: &'a mut dyn FnMut(&str) -> Result<String>,
    ns: Namespace,
    resolving: Vec<String>,
}

impl<'a> Ctx<'a> {
    fn seed_facts(&mut self) {
        let f = self.facts;
        let plain = |value: String| {
            Ok(Resolved {
                value,
                secret: false,
            })
        };
        self.ns.insert("name".into(), plain(f.name.clone()));
        self.ns.insert("scale".into(), plain(f.scale.to_string()));
        self.ns.insert("arch".into(), plain(f.arch.clone()));
        if let Some(v) = &f.version {
            self.ns.insert("version".into(), plain(v.clone()));
        }
        if let Some(v) = &f.image {
            self.ns.insert("image".into(), plain(v.clone()));
        }
        if let Some(h) = &f.host {
            self.ns.insert("host".into(), plain(h.clone()));
        }
        if let Some(p) = f.port {
            self.ns.insert("port".into(), plain(p.to_string()));
        }
        if let (Some(h), Some(p)) = (&f.host, f.port) {
            self.ns.insert("addr".into(), plain(format!("{h}:{p}")));
            self.ns
                .insert("base_url".into(), plain(format!("http://{h}:{p}")));
        }
    }

    /// Resolve `name` (a declared param or already-seeded fact), memoizing
    /// into `self.ns`. Computed (hole-bearing) values recurse through this
    /// same method; a depth cap plus an explicit in-progress check catch
    /// cycles that stack-supplied overrides could introduce (manifest-level
    /// cycle detection, Task 2, only covers the image's own declared
    /// defaults).
    fn resolve(&mut self, name: &str) -> Result<Resolved> {
        // An already-CAPTURED failure (a computed param this pass gave up on
        // earlier) is reported again, verbatim, to whatever now reads it —
        // exactly what an eager resolution would have surfaced.
        match self.ns.get(name) {
            Some(Ok(r)) => return Ok(r.clone()),
            Some(Err(msg)) => return Err(Error::Manifest(msg.clone())),
            None => {}
        }
        if LIVE.contains(&name) {
            return Err(live_error(&self.facts.name, name));
        }
        let Some(decl) = self.decls.get(name) else {
            return Err(no_such_param(
                &self.facts.name,
                name,
                self.facts.host.is_some(),
                self.decls.keys().map(String::as_str),
            ));
        };

        if self.resolving.iter().any(|n| n == name) || self.resolving.len() > self.decls.len() {
            let mut cycle = self.resolving.clone();
            cycle.push(name.to_string());
            return Err(Error::Manifest(format!(
                "{}: params cycle: {}",
                self.facts.name,
                cycle.join(" → ")
            )));
        }

        self.resolving.push(name.to_string());
        let resolved = match decl {
            ParamDecl::Secret { .. } => {
                let value = match self.overrides.get(name) {
                    Some(ov) => ov.clone(),
                    None => (self.secrets)(name)?,
                };
                Ok(Resolved {
                    value,
                    secret: true,
                })
            }
            ParamDecl::Value(v) => {
                let source = self
                    .overrides
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| v.clone());
                let who = format!("{}.{name}", self.facts.name);
                parse_template(&source, &who).and_then(|pieces| {
                    interpolate(&pieces, &mut |pref: &PRef| {
                        if let Some(app) = &pref.app {
                            return Err(Error::Manifest(format!(
                                "{}: `{{{app}.{}}}` — a member's own params can only reference \
                                 its own namespace here; cross-member refs resolve when the stack wires env",
                                self.facts.name, pref.param
                            )));
                        }
                        self.resolve(&pref.param)
                    })
                })
            }
        };
        self.resolving.pop();
        let resolved = resolved?;
        self.ns.insert(name.to_string(), Ok(resolved.clone()));
        Ok(resolved)
    }

    /// Record `name` as unresolvable, keeping the message the failure
    /// produced (never a value — every error built here names params and
    /// members only). [`lookup`] replays it if anything reads the param.
    fn capture(&mut self, name: &str, e: Error) {
        let msg = match e {
            Error::Manifest(msg) => msg,
            other => other.to_string(),
        };
        self.ns.insert(name.to_string(), Err(msg));
    }
}

/// Build a member's resolved parameter namespace: built-in facts (never
/// secret), declared params (stack override wins over the manifest default;
/// secrets are minted/loaded via `secrets` unless overridden), and computed
/// params (values containing holes) resolved recursively against the
/// namespace itself.
///
/// Two passes, and they differ on purpose:
///
/// - **Secrets are eager.** Minting is a side effect (a 0600 file in the
///   stack's store) and a missing EXTERNAL secret is a refusal — both must
///   happen whether or not anything reads the param, so a `Secret` decl that
///   cannot resolve fails the whole call, unchanged.
/// - **Values are lazy-ish.** A `Value` decl that cannot resolve — a
///   computed `url = "…{host}:{port}…"` on a run that publishes nothing, the
///   common shape in a keg's `[params]` — is CAPTURED in the returned
///   [`Namespace`] rather than failing it. Nothing has read it yet;
///   [`lookup`] names the gap, with the same message, if something does.
pub fn namespace(
    facts: &MemberFacts,
    decls: &BTreeMap<String, ParamDecl>,
    overrides: &BTreeMap<String, String>,
    secrets: &mut dyn FnMut(&str) -> Result<String>,
) -> Result<Namespace> {
    let mut ctx = Ctx {
        facts,
        decls,
        overrides,
        secrets,
        ns: BTreeMap::new(),
        resolving: Vec::new(),
    };
    ctx.seed_facts();
    // Secrets first, so a computed value that embeds one never captures a
    // minting failure (or an external secret's refusal) as its own.
    for (name, decl) in decls {
        if matches!(decl, ParamDecl::Secret { .. }) {
            ctx.resolve(name)?;
        }
    }
    for (name, decl) in decls {
        if matches!(decl, ParamDecl::Value(_)) {
            if let Err(e) = ctx.resolve(name) {
                ctx.capture(name, e);
            }
        }
    }
    Ok(ctx.ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resolved param, for the common `ns["x"].value` assertion.
    fn val<'a>(ns: &'a Namespace, name: &str) -> &'a Resolved {
        ns[name]
            .as_ref()
            .unwrap_or_else(|e| panic!("{name} did not resolve: {e}"))
    }

    /// The message a param CAPTURED instead of resolving — the failure
    /// `lookup` replays to whatever reads it.
    fn gap<'a>(ns: &'a Namespace, name: &str) -> &'a str {
        ns[name]
            .as_ref()
            .err()
            .unwrap_or_else(|| panic!("{name} resolved, expected a captured failure"))
    }

    fn lit(pieces: &[Piece]) -> String {
        let mut r = interpolate(pieces, &mut |p: &PRef| {
            Ok(Resolved {
                value: format!("<{}.{}>", p.app.as_deref().unwrap_or("self"), p.param),
                secret: p.param == "password",
            })
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
        let r = interpolate(&p, &mut |pr: &PRef| {
            Ok(Resolved {
                value: pr.param.clone(),
                secret: pr.param == "password",
            })
        })
        .unwrap();
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
        assert_eq!(
            r.iter().filter(|p| p.app.as_deref() == Some("db")).count(),
            2
        );
    }

    #[test]
    fn decl_shapes() {
        let v: toml::Value = toml::from_str("x = \"postgres\"").unwrap();
        assert!(matches!(
            ParamDecl::from_toml("x", &v["x"], "t").unwrap(),
            ParamDecl::Value(_)
        ));
        let v: toml::Value = toml::from_str("x = { secret = true }").unwrap();
        assert!(matches!(
            ParamDecl::from_toml("x", &v["x"], "t").unwrap(),
            ParamDecl::Secret { external: false }
        ));
        let v: toml::Value = toml::from_str("x = { secret = true, external = false }").unwrap();
        assert!(matches!(
            ParamDecl::from_toml("x", &v["x"], "t").unwrap(),
            ParamDecl::Secret { external: false }
        ));
        let v: toml::Value = toml::from_str("x = { secret = true, default = \"no\" }").unwrap();
        assert!(ParamDecl::from_toml("x", &v["x"], "t").is_err());
    }

    #[test]
    fn namespace_builds_facts_and_computed_url() {
        let facts = MemberFacts {
            name: "db".into(),
            version: Some("17.10.3".into()),
            host: Some("db.ply".into()),
            port: Some(5432),
            scale: 1,
            arch: "x64".into(),
            image: None,
        };
        let mut decls = BTreeMap::new();
        decls.insert("user".into(), ParamDecl::Value("postgres".into()));
        decls.insert("database".into(), ParamDecl::Value("postgres".into()));
        decls.insert("password".into(), ParamDecl::Secret { external: false });
        decls.insert(
            "url".into(),
            ParamDecl::Value("postgres://{user}:{password}@{host}:{port}/{database}".into()),
        );
        let mut overrides = BTreeMap::new();
        overrides.insert("database".into(), "todos".into());
        let ns = namespace(&facts, &decls, &overrides, &mut |_| Ok("S3CR3T".into())).unwrap();
        assert_eq!(val(&ns, "base_url").value, "http://db.ply:5432");
        assert_eq!(
            val(&ns, "url").value,
            "postgres://postgres:S3CR3T@db.ply:5432/todos"
        );
        assert!(
            val(&ns, "url").secret && !val(&ns, "database").secret,
            "taint follows the password into url"
        );
    }

    /// The gap is named when the param is READ (C1) — and `{port}` on a
    /// member that has an address but publishes nothing still reads as the
    /// publish gap it is.
    #[test]
    fn port_ref_without_publish_names_the_gap() {
        let facts = MemberFacts {
            name: "job".into(),
            version: None,
            host: Some("job.ply".into()),
            port: None,
            scale: 1,
            arch: "x64".into(),
            image: None,
        };
        let mut decls = BTreeMap::new();
        decls.insert(
            "url".into(),
            ParamDecl::Value("http://{host}:{port}".into()),
        );
        let ns = namespace(&facts, &decls, &BTreeMap::new(), &mut |_| unreachable!()).unwrap();
        let e = lookup(
            &ns,
            &PRef {
                app: None,
                param: "url".into(),
            },
            "job",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("publishes no port"), "{e}");
        assert_eq!(gap(&ns, "url"), "job: `{port}` — job publishes no port");
    }

    #[test]
    fn namespace_facts_only_when_no_publish() {
        let facts = MemberFacts {
            name: "job".into(),
            version: None,
            host: None,
            port: None,
            scale: 3,
            arch: "x64".into(),
            image: None,
        };
        let ns = namespace(
            &facts,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &mut |_| unreachable!(),
        )
        .unwrap();
        assert_eq!(val(&ns, "name").value, "job");
        assert_eq!(val(&ns, "scale").value, "3");
        assert!(!ns.contains_key("host"));
        assert!(!ns.contains_key("addr"));
        assert!(!ns.contains_key("base_url"));
    }

    #[test]
    fn override_supplants_minting_and_stays_secret() {
        let facts = MemberFacts {
            name: "api".into(),
            version: None,
            host: None,
            port: None,
            scale: 1,
            arch: "x64".into(),
            image: None,
        };
        let mut decls = BTreeMap::new();
        decls.insert("key".into(), ParamDecl::Secret { external: true });
        let mut overrides = BTreeMap::new();
        overrides.insert("key".into(), "sk_live_x".into());
        let ns = namespace(&facts, &decls, &overrides, &mut |_| unreachable!()).unwrap();
        assert_eq!(val(&ns, "key").value, "sk_live_x");
        assert!(
            val(&ns, "key").secret,
            "secret decls stay tainted even overridden"
        );
    }

    #[test]
    fn unresolvable_ref_names_the_declared_list() {
        let facts = MemberFacts {
            name: "web".into(),
            version: None,
            host: None,
            port: None,
            scale: 1,
            arch: "x64".into(),
            image: None,
        };
        let mut decls = BTreeMap::new();
        decls.insert("greeting".into(), ParamDecl::Value("hi {typo}".into()));
        let ns = namespace(&facts, &decls, &BTreeMap::new(), &mut |_| unreachable!()).unwrap();
        let e = lookup(
            &ns,
            &PRef {
                app: None,
                param: "greeting".into(),
            },
            "web",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("`{typo}` is not a param of web"), "{e}");
        assert!(e.contains("declared: greeting"), "{e}");
    }

    #[test]
    fn stack_supplied_override_cycle_is_caught() {
        let facts = MemberFacts {
            name: "web".into(),
            version: None,
            host: None,
            port: None,
            scale: 1,
            arch: "x64".into(),
            image: None,
        };
        let mut decls = BTreeMap::new();
        decls.insert("a".into(), ParamDecl::Value("x".into()));
        decls.insert("b".into(), ParamDecl::Value("y".into()));
        let mut overrides = BTreeMap::new();
        overrides.insert("a".into(), "{b}".into());
        overrides.insert("b".into(), "{a}".into());
        // Captured, not thrown (C1) — the cycle is still caught, and named
        // to whatever reads either end of it.
        let ns = namespace(&facts, &decls, &overrides, &mut |_| unreachable!()).unwrap();
        assert!(gap(&ns, "a").contains("cycle"), "{}", gap(&ns, "a"));
        let e = lookup(
            &ns,
            &PRef {
                app: None,
                param: "a".into(),
            },
            "web",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("cycle"), "{e}");
    }

    #[test]
    fn a_secret_resolved_value_never_prints_in_debug_output() {
        let secret = Resolved {
            value: "s3cr3t-value".to_string(),
            secret: true,
        };
        let debug = format!("{secret:?}");
        assert!(debug.contains("********"), "{debug}");
        assert!(!debug.contains("s3cr3t-value"), "{debug}");

        let plain = Resolved {
            value: "todos".to_string(),
            secret: false,
        };
        assert!(format!("{plain:?}").contains("todos"));
    }

    #[test]
    fn lookup_rejects_live_names() {
        let mut ns = BTreeMap::new();
        ns.insert(
            "url".into(),
            Ok(Resolved {
                value: "postgres://x".into(),
                secret: false,
            }),
        );
        let r = PRef {
            app: Some("db".into()),
            param: "state".into(),
        };
        let e = lookup(&ns, &r, "db").unwrap_err().to_string();
        assert!(e.contains("is live"), "{e}");

        let r = PRef {
            app: Some("db".into()),
            param: "url".into(),
        };
        assert_eq!(lookup(&ns, &r, "db").unwrap().value, "postgres://x");
    }

    /// C1: a computed param that reads a fact this run doesn't have (a keg's
    /// `url = "redis://{host}:{port}"` under a bare rootless `ply run`) must
    /// not take the whole namespace down with it — nothing referenced it.
    /// The gap is named when, and only when, something reads it.
    #[test]
    fn an_unreferenced_computed_param_that_reads_an_absent_fact_resolves_the_rest() {
        let facts = MemberFacts {
            name: "redis".into(),
            version: None,
            host: None,
            port: None,
            scale: 1,
            arch: "x64".into(),
            image: None,
        };
        let mut decls = BTreeMap::new();
        decls.insert("greeting".into(), ParamDecl::Value("hi".into()));
        decls.insert(
            "url".into(),
            ParamDecl::Value("redis://{host}:{port}".into()),
        );
        let ns = namespace(&facts, &decls, &BTreeMap::new(), &mut |_| unreachable!())
            .expect("an unreferenced computed param must not fail the namespace");
        assert_eq!(val(&ns, "greeting").value, "hi");

        let r = PRef {
            app: None,
            param: "url".into(),
        };
        let e = lookup(&ns, &r, "redis").unwrap_err().to_string();
        assert!(e.contains("redis.ply"), "{e}");
    }

    /// M2: `{host}`/`{addr}`/`{base_url}` on a run with no `<name>.ply`
    /// address get their own sentence — "publishes no port" is about
    /// `{port}`, and says nothing useful about a rootless run's missing
    /// address.
    #[test]
    fn a_missing_address_reads_differently_from_a_missing_port() {
        let facts = MemberFacts {
            name: "app".into(),
            version: None,
            host: None,
            port: None,
            scale: 1,
            arch: "x64".into(),
            image: None,
        };
        let ns = namespace(
            &facts,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &mut |_| unreachable!(),
        )
        .unwrap();
        let e = lookup(
            &ns,
            &PRef {
                app: None,
                param: "host".into(),
            },
            "app",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("no `app.ply` address"), "{e}");
        assert!(e.contains("rootful"), "{e}");
        assert!(!e.contains("publishes no port"), "{e}");
    }
}
