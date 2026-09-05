//! A stack: several `ply run`s, written down. The compose equivalent.
//!
//! A stack file is pure wiring — it builds no image of its own. It is a
//! `[stack]` metadata header (name/description) followed by an `[[app]]`
//! array, and **each `[[app]]` block is exactly one `ply run`**: its fields
//! map 1:1 to run flags (`run`→the image, `name`→`--name`, `e`→`-e`,
//! `publish`→`--publish`, `after`→`--after`, `volume`→`--volume`,
//! `domain`→`--domain`, `scale`→`--scale`). There is no stack concept beyond
//! "these runs, in dependency order."
//!
//! `after` edges name other members (by their `name`) and ride the existing
//! `--after` readiness gate. `$VAR` in an `e` value is substituted from the
//! environment at launch — undefined is a hard error, never a silent empty
//! (see `expand_vars`).
//!
//! Registry members are version-locked in the stack dir's ply.lock — upgrades
//! are deliberate (`ply up --refresh`), same principle as MVS.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::manifest::Manifest;
use crate::params::{self, MemberFacts, PRef, Piece, Resolved};
use crate::secrets::SecretStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberSource {
    /// `run = "postgres@17"` — a runnable app fetched from the registry.
    Run {
        name: String,
        version: Option<String>,
    },
    /// `run = "./server"` — a local app dir (or `.img` file), built/run at
    /// `up` time exactly like a hand-typed `ply run ./server`.
    Path(PathBuf),
    /// `run = "https://…/app.img"` — a URL image, fetched like `ply run <url>`.
    Url(String),
    /// `run = "docker://image:tag"` — an OCI image, imported at `up` time
    /// through the same cache `ply run docker://` uses (pinned to the first
    /// pull; `--refresh` pulls again), then run like an `.img` member.
    Docker(String),
}

#[derive(Debug, Clone)]
pub struct Member {
    /// The member identity — the `--name` the instance runs under, and the
    /// handle `after` edges name. Defaults to the image basename of `run`.
    pub name: String,
    pub source: MemberSource,
    /// `-e KEY=VALUE`; VALUE may contain `$VAR`, expanded at launch.
    pub env: Vec<(String, String)>,
    /// `params = { key = "value" }`; VALUE may contain `$VAR` (expanded like
    /// `env`) or `{param}`/`{app.param}` holes, resolved when the stack wires
    /// this member's params namespace.
    pub params: Vec<(String, String)>,
    /// Member names this one waits for → `--after`.
    pub after: Vec<String>,
    /// `--publish` entries.
    pub publish: Vec<String>,
    /// `--volume` paths.
    pub volume: Vec<String>,
    /// `--domain` entries.
    pub domain: Vec<String>,
    /// `--scale`.
    pub scale: Option<u32>,
    /// `egress = "audit"` or `egress = { mode = …, allow = [...] }` — the
    /// operator's word on what this member may reach, over whatever its
    /// image's manifest declares. `allow` REPLACES the manifest's list.
    pub egress: Option<crate::egress::EgressOverride>,
}

#[derive(Debug)]
pub struct Stack {
    pub name: Option<String>,
    /// Registry namespace this stack publishes under (`<owner>/<name>`).
    /// Absent for a local/unpublished stack.
    pub owner: Option<String>,
    /// Semver of the stack as a publishable artifact (required only to
    /// `ply push` it; optional for local `ply up`).
    pub version: Option<String>,
    pub description: Option<String>,
    /// A root-owned env file whose KEY=VALUE lines fill `$VAR` holes when a
    /// host expands this stack (the deploy-time equivalent of `ply up
    /// --env-file`). Relative paths resolve against the deployments dir.
    pub env_file: Option<String>,
    /// Members in dependency order (dependencies before dependants).
    pub members: Vec<Member>,
}

/// Read `<dir>/ply.toml` as a stack. `Ok(None)` when the file has no
/// `[[app]]` array (it's an app manifest or absent).
pub fn load(dir: &Path) -> Result<Option<Stack>> {
    let path = dir.join("ply.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    parse(&text, &path)
}

/// The stack `ply up DIR` should start: `stack.toml` first, then a
/// `ply.toml` that carries `[[app]]`.
///
/// Two lookups, not one, because the answer differs by question. A repo
/// whose root is an APP (its own ply.toml) may also ship a stack.toml
/// describing the deployment it belongs to — ply-web is exactly that. For
/// `ply up` the stack.toml is the point; for "is this directory a stack?"
/// (`ply run DIR`) it is not — that directory is still an app, and `load`
/// keeps answering from ply.toml alone.
pub fn discover(dir: &Path) -> Result<Option<(Stack, PathBuf)>> {
    for candidate in ["stack.toml", "ply.toml"] {
        let path = dir.join(candidate);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(stack) = parse(&text, &path)? {
            return Ok(Some((stack, path)));
        }
    }
    Ok(None)
}

/// Local overrides for a stack: `stack.toml` → `stack.dev.toml` beside it,
/// the stack-level twin of `ply.dev.toml`.
///
/// A committed stack describes production — members reach each other by
/// their `<name>.ply` bridge names, secrets are `$VAR` holes. A laptop
/// differs in fewer ways than it used to — a rootless stack gets its own
/// network, so `<name>.ply` and the members' real ports mean the same thing
/// there. What is still local is a dev password, and a published port that
/// may have to dodge whatever the machine already runs. The overlay is where
/// those local truths live, so the production file never has to carry a
/// dev-shaped lie.
///
/// Applied by `ply up` only. A host reconciling a deployment never reads it
/// — same rule as `ply.dev.toml`, and the reason a stack stays publishable.
pub fn dev_overlay_path(stack_file: &Path) -> PathBuf {
    let name = stack_file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = name.strip_suffix(".toml").unwrap_or(&name);
    stack_file.with_file_name(format!("{stem}.dev.toml"))
}

/// Apply `<stack>.dev.toml` if it exists. Returns a summary of what it
/// changed (for the "applying …" line), or None when there is no overlay.
pub fn apply_dev_overlay(stack: &mut Stack, stack_file: &Path) -> Result<Option<String>> {
    let path = dev_overlay_path(stack_file);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let overlay = parse_overlay(&text, &path)?;
    let mut touched: Vec<String> = Vec::new();

    if let Some(env_file) = overlay.env_file {
        stack.env_file = Some(env_file);
        touched.push("env_file".into());
    }
    for member in overlay.members {
        let Some(target) = stack.members.iter_mut().find(|m| m.name == member.name) else {
            // a typo must not silently do nothing — the whole point of an
            // overlay is that it changes something
            return Err(Error::Manifest(format!(
                "{}: no member named `{}` in {} — overlays override members, they do not add them",
                path.display(),
                member.name,
                stack_file.display()
            )));
        };
        let mut fields: Vec<&str> = Vec::new();
        if let Some(run) = member.run {
            let (source, _) = classify_run(&run, &path, 0)?;
            target.source = source;
            fields.push("run");
        }
        if !member.env.is_empty() {
            // merged by key: an overlay adds DATABASE_URL without discarding
            // whatever else the member declares
            for (k, v) in member.env {
                match target.env.iter_mut().find(|(key, _)| key == &k) {
                    Some(pair) => pair.1 = v,
                    None => target.env.push((k, v)),
                }
            }
            fields.push("env");
        }
        if !member.params.is_empty() {
            // merged by key, same rule as env
            for (k, v) in member.params {
                match target.params.iter_mut().find(|(key, _)| key == &k) {
                    Some(pair) => pair.1 = v,
                    None => target.params.push((k, v)),
                }
            }
            fields.push("params");
        }
        if let Some(publish) = member.publish {
            target.publish = publish;
            fields.push("publish");
        }
        if let Some(domain) = member.domain {
            target.domain = domain;
            fields.push("domain");
        }
        if let Some(volume) = member.volume {
            target.volume = volume;
            fields.push("volume");
        }
        if let Some(scale) = member.scale {
            target.scale = Some(scale);
            fields.push("scale");
        }
        if !fields.is_empty() {
            touched.push(format!("{}({})", member.name, fields.join(",")));
        }
    }
    Ok(Some(touched.join(", ")))
}

/// One member's overrides. Every field is optional but `name`, which says
/// WHICH member is being overridden.
struct MemberOverlay {
    name: String,
    run: Option<String>,
    env: Vec<(String, String)>,
    params: Vec<(String, String)>,
    publish: Option<Vec<String>>,
    domain: Option<Vec<String>>,
    volume: Option<Vec<String>>,
    scale: Option<u32>,
}

struct StackOverlay {
    env_file: Option<String>,
    members: Vec<MemberOverlay>,
}

fn parse_overlay(text: &str, path: &Path) -> Result<StackOverlay> {
    let doc: toml::Value = text
        .parse()
        .map_err(|e| Error::Manifest(format!("{}: {e}", path.display())))?;
    let env_file = doc
        .get("stack")
        .and_then(|s| s.get("env_file"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let mut members = Vec::new();
    for (i, entry) in doc
        .get("app")
        .and_then(|a| a.as_array())
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let table = entry.as_table().ok_or_else(|| {
            Error::Manifest(format!(
                "{}: [[app]] #{} is not a table",
                path.display(),
                i + 1
            ))
        })?;
        let name = table
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                Error::Manifest(format!(
                    "{}: [[app]] #{} needs `name` — it says which member to override",
                    path.display(),
                    i + 1
                ))
            })?
            .to_string();
        let list = |key: &str| -> Result<Option<Vec<String>>> {
            match table.get(key) {
                None => Ok(None),
                Some(v) => Ok(Some(string_list(Some(v), key, &name, path)?)),
            }
        };
        members.push(MemberOverlay {
            run: table
                .get("run")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            env: parse_env(member_env(table)?, &name, path)?,
            params: parse_params(table.get("params"), &name, path)?,
            publish: list("publish")?,
            domain: list("domain")?,
            volume: list("volume")?,
            scale: table
                .get("scale")
                .and_then(|v| v.as_integer())
                .map(|n| n as u32),
            name,
        });
    }
    Ok(StackOverlay { env_file, members })
}

/// Parse a stack file at `path`. `Ok(None)` when there is no `[[app]]` array
/// — including `app = "name"` as a plain string, which is the single-app
/// DEPLOYMENT lane's registry key, not a malformed stack. Only an array of
/// `[[app]]` tables makes a file a stack.
/// A deployment file that NAMES a published stack instead of spelling one
/// out: `stack = "<namespace>/<name>"`. Always newest — the reference carries
/// no version, so reconcile re-fetches every beat and a republished SHAPE (a
/// member added, a port moved, an `after` edge changed) converges the same
/// way a new member image does.
///
/// `env_file` fills the published stack's `$VAR` holes on this host — the
/// deployer's secrets for the publisher's template. `auto = false` pins the
/// shape until the file is touched, exactly as on a single-app spec.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackRef {
    #[serde(rename = "stack")]
    pub reference: String,
    #[serde(default)]
    pub env_file: Option<String>,
    #[serde(default = "default_true")]
    pub auto: bool,
}

fn default_true() -> bool {
    true
}

/// Is this deployment file a stack REFERENCE?
///
/// Decided by positive shape: the file deserialises as a `StackRef` — `stack`
/// plus at most `env_file` and `auto` — and nothing else. `stack` is also the
/// cosmetic grouping label on a single-app spec, so the first version of this
/// check used a denylist of source keys, which meant a misspelled `form =`
/// still hijacked the file into this lane and tore the app down. Any key a
/// reference does not own now sends the file to `Spec::parse`, whose
/// `deny_unknown_fields` error names the typo.
pub fn parse_ref(text: &str) -> Option<StackRef> {
    let r: StackRef = toml::from_str(text).ok()?;
    (!r.reference.trim().is_empty()).then_some(r)
}

pub fn parse(text: &str, path: &Path) -> Result<Option<Stack>> {
    let doc: toml::Value = text
        .parse()
        .map_err(|e| Error::Manifest(format!("{}: {e}", path.display())))?;
    let Some(apps) = doc.get("app").and_then(|a| a.as_array()) else {
        return Ok(None);
    };
    if doc.get("package").is_some() {
        return Err(Error::Manifest(format!(
            "{}: has both [package] and [[app]] — a stack file is pure wiring; move the app into its own directory and reference it with `run = \"./dir\"`",
            path.display()
        )));
    }
    if apps.is_empty() {
        return Err(Error::Manifest(format!(
            "{}: [[app]] has no entries",
            path.display()
        )));
    }

    let (mut name, mut owner, mut version, mut description, mut env_file) =
        (None, None, None, None, None);
    if let Some(meta) = doc.get("stack") {
        let meta = meta.as_table().ok_or_else(|| {
            Error::Manifest(format!("{}: [stack] must be a table", path.display()))
        })?;
        for key in meta.keys() {
            if !matches!(
                key.as_str(),
                "name" | "owner" | "version" | "description" | "env_file"
            ) {
                return Err(Error::Manifest(format!(
                    "{}: [stack] has unknown key `{key}` (expected name, owner, version, description, env_file)",
                    path.display()
                )));
            }
        }
        name = meta
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        owner = meta
            .get("owner")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        version = meta
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        description = meta
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        env_file = meta
            .get("env_file")
            .and_then(|v| v.as_str())
            .map(str::to_string);
    }

    let mut members = Vec::new();
    for (i, entry) in apps.iter().enumerate() {
        members.push(parse_member(i, entry, path)?);
    }

    // member names must be unique
    let mut seen = BTreeSet::new();
    for member in &members {
        if !seen.insert(member.name.as_str()) {
            return Err(Error::Manifest(format!(
                "{}: two members named `{}` — give one a distinct `name`",
                path.display(),
                member.name
            )));
        }
    }

    // after edges must parse as a `--after` condition (Task 8's grammar —
    // `member`, `member.param`, or `member.param == '…'`) and must name a
    // member. `Member.after` keeps the raw string regardless — `ply up`
    // passes the whole condition through to `--after` unchanged — but
    // ordering and these checks key on the parsed `wait.app`.
    let names: BTreeSet<&str> = members.iter().map(|m| m.name.as_str()).collect();
    for member in &members {
        for dep in &member.after {
            let wait = crate::runtime::after::parse_wait(dep).map_err(|e| {
                Error::Manifest(format!("{}: member `{}`: {e}", path.display(), member.name))
            })?;
            if !names.contains(wait.app.as_str()) {
                return Err(Error::Manifest(format!(
                    "{}: member `{}` waits for `{}`, which is not a member",
                    path.display(),
                    member.name,
                    wait.app
                )));
            }
            if wait.app == member.name {
                return Err(Error::Manifest(format!(
                    "{}: member `{}` waits for itself",
                    path.display(),
                    member.name
                )));
            }
        }
    }

    // `{app.param}` refs in env/publish/domain must name a real member, and
    // must not target a live (runtime-only) param — those are never
    // resolvable statically, see params::LIVE. `{self.x}` and a ref naming
    // the member itself are its own params namespace, not a cross-member
    // ref, and are not checked here.
    for member in &members {
        let values = member
            .env
            .iter()
            .map(|(_, v)| v)
            .chain(member.publish.iter())
            .chain(member.domain.iter());
        for v in values {
            for r in params::refs(v) {
                let PRef { app, param } = r;
                let Some(app) = app else { continue };
                if app == member.name || app == "self" {
                    continue;
                }
                if !names.contains(app.as_str()) {
                    return Err(Error::Manifest(format!(
                        "{}: `{{{app}.{param}}}` references no stack member named `{app}`",
                        member.name
                    )));
                }
                if params::LIVE.contains(&param.as_str()) {
                    return Err(Error::Manifest(format!(
                        "{}: `{{{app}.{param}}}` is live — wait on it with \
                         `after = [\"{app}.{param} == '…'\"]`, or read /run/ply/{app}/{param} at runtime",
                        member.name
                    )));
                }
            }
        }
    }

    // `{}` interpolation is a v1 feature of `e =` values and `params =`
    // overrides only: `publish`/`domain` are scanned for edges but never
    // interpolated, so a hole there would reach `parse_publish` raw as the
    // literal text `{db.port}`. Say so at parse rather than fail obscurely
    // (or, worse, publish a nonsense port) at launch.
    for member in &members {
        for (what, values) in [("publish", &member.publish), ("domain", &member.domain)] {
            for v in values {
                let Some(r) = params::refs(v).into_iter().next() else {
                    continue;
                };
                let hole = match &r.app {
                    Some(app) => format!("{{{app}.{}}}", r.param),
                    None => format!("{{{}}}", r.param),
                };
                return Err(Error::Manifest(format!(
                    "{}: member `{}`: `{what}` value `{v}` has a `{hole}` hole — v1 \
                     interpolates `{{}}` in `e =` values and `params =` overrides only; \
                     write the port literally (use `$VAR` for a deploy-time value)",
                    path.display(),
                    member.name
                )));
            }
        }
    }

    Ok(Some(Stack {
        name,
        owner,
        version,
        description,
        env_file,
        members: topo_sort(members, path)?,
    }))
}

const MEMBER_KEYS: &[&str] = &[
    "run", "name", "env", "e", "after", "publish", "volume", "domain", "scale", "params", "egress",
];

/// A member's environment: `env` is the spelling, `e` the original alias.
/// Every other member key is written out in full (`publish`, `domain`,
/// `volume`, `scale`), so `e` — borrowed from the `-e` flag — was the one
/// key a reader had to look up. Both are accepted; naming both is an error
/// rather than a silent winner.
fn member_env(table: &toml::value::Table) -> Result<Option<&toml::Value>> {
    match (table.get("env"), table.get("e")) {
        (Some(_), Some(_)) => Err(Error::Manifest(
            "a member sets both `env` and `e` — they are the same key; keep `env`".into(),
        )),
        (some @ Some(_), None) | (None, some) => Ok(some),
    }
}

fn parse_member(index: usize, entry: &toml::Value, path: &Path) -> Result<Member> {
    let table = entry.as_table().ok_or_else(|| {
        Error::Manifest(format!(
            "{}: [[app]] #{} must be a table, e.g. run = \"postgres@17\"",
            path.display(),
            index + 1
        ))
    })?;
    for key in table.keys() {
        if !MEMBER_KEYS.contains(&key.as_str()) {
            return Err(Error::Manifest(format!(
                "{}: [[app]] #{}: unknown key `{key}` (expected {})",
                path.display(),
                index + 1,
                MEMBER_KEYS.join(", ")
            )));
        }
    }

    let run = table
        .get("run")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Error::Manifest(format!(
                "{}: [[app]] #{} needs `run` — the image: a registry ref (`postgres@17`), a path (`./dir`), or a URL",
                path.display(),
                index + 1
            ))
        })?;
    let (source, default_name) = classify_run(run, path, index)?;

    let name = match table.get("name") {
        Some(v) => v
            .as_str()
            .ok_or_else(|| {
                Error::Manifest(format!(
                    "{}: [[app]] #{}: `name` must be a string",
                    path.display(),
                    index + 1
                ))
            })?
            .to_string(),
        None => default_name.ok_or_else(|| {
            Error::Manifest(format!(
                "{}: [[app]] #{}: cannot derive a name from `run = \"{run}\"` — add `name = \"…\"`",
                path.display(),
                index + 1
            ))
        })?,
    };

    let env = parse_env(member_env(table)?, &name, path)?;
    let params = parse_params(table.get("params"), &name, path)?;
    let after = string_list(table.get("after"), "after", &name, path)?;
    let publish = string_list(table.get("publish"), "publish", &name, path)?;
    let volume = string_list(table.get("volume"), "volume", &name, path)?;
    let domain = string_list(table.get("domain"), "domain", &name, path)?;
    let scale = match table.get("scale") {
        None => None,
        Some(toml::Value::Integer(n)) if *n > 0 => Some(*n as u32),
        Some(_) => {
            return Err(Error::Manifest(format!(
                "{}: member `{name}`: `scale` must be a positive integer",
                path.display()
            )))
        }
    };

    let egress = match table.get("egress") {
        None => None,
        Some(toml::Value::String(mode)) => Some(crate::egress::EgressOverride {
            mode: Some(mode.parse().map_err(|e| {
                Error::Manifest(format!("{}: member `{name}`: {e}", path.display()))
            })?),
            allow: None,
        }),
        Some(toml::Value::Table(t)) => {
            for key in t.keys() {
                if key != "mode" && key != "allow" {
                    return Err(Error::Manifest(format!(
                        "{}: member `{name}`: `egress` accepts `mode` and `allow`, not `{key}`",
                        path.display()
                    )));
                }
            }
            let mode = match t.get("mode") {
                None => None,
                Some(toml::Value::String(m)) => Some(m.parse().map_err(|e| {
                    Error::Manifest(format!("{}: member `{name}`: {e}", path.display()))
                })?),
                Some(_) => {
                    return Err(Error::Manifest(format!(
                        "{}: member `{name}`: `egress.mode` must be a string",
                        path.display()
                    )))
                }
            };
            let allow = match t.get("allow") {
                None => None,
                Some(v) => {
                    let raw = string_list(Some(v), "egress.allow", &name, path)?;
                    Some(crate::egress::entry::parse_list(&raw).map_err(|e| {
                        Error::Manifest(format!("{}: member `{name}`: {e}", path.display()))
                    })?)
                }
            };
            Some(crate::egress::EgressOverride { mode, allow })
        }
        Some(_) => {
            return Err(Error::Manifest(format!(
                "{}: member `{name}`: `egress` must be a mode string (\"off\" | \"audit\" | \"enforce\") or a table {{ mode = …, allow = [...] }}",
                path.display()
            )))
        }
    };

    Ok(Member {
        name,
        source,
        env,
        params,
        after,
        publish,
        volume,
        domain,
        scale,
        egress,
    })
}

/// Classify a `run =` value into a source, returning the default member name.
/// The member name a `docker://` reference implies: the image's last path
/// segment without tag or digest — `docker://ghcr.io/org/app:1.2` → `app`.
pub fn docker_member_name(reference: &str) -> Option<String> {
    let rest = reference.strip_prefix("docker://")?;
    let last = rest.rsplit('/').next()?;
    let name = last.split('@').next()?.split(':').next()?;
    (!name.is_empty()).then(|| name.to_string())
}

fn classify_run(run: &str, path: &Path, index: usize) -> Result<(MemberSource, Option<String>)> {
    if run.starts_with("docker://") {
        return Ok((
            MemberSource::Docker(run.to_string()),
            docker_member_name(run),
        ));
    }
    if run.starts_with("http://") || run.starts_with("https://") {
        let stem = run
            .rsplit('/')
            .next()
            .and_then(|f| f.split('-').next())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        return Ok((MemberSource::Url(run.to_string()), stem));
    }
    if run.starts_with("./")
        || run.starts_with("../")
        || run.starts_with('/')
        || run.ends_with(".img")
    {
        let stem = Path::new(run)
            .file_name()
            .and_then(|f| f.to_str())
            .map(|f| f.trim_end_matches(".img"))
            .and_then(|f| f.split('-').next().filter(|s| !s.is_empty()))
            .map(str::to_string);
        return Ok((MemberSource::Path(PathBuf::from(run)), stem));
    }
    match crate::catalog::parse_namespaced_ref(run) {
        Some((name, version)) => {
            // The member's default name is the package, without its
            // namespace: `ply/ply-web` runs as `ply-web`, which is what
            // `after`, the `<member>.ply` bridge name and `ply ps` show.
            let member = name.rsplit('/').next().unwrap_or(&name).to_string();
            Ok((
                MemberSource::Run {
                    name: name.clone(),
                    version,
                },
                Some(member),
            ))
        }
        None => Err(Error::Manifest(format!(
            "{}: [[app]] #{}: `run = \"{run}\"` is not a package reference, path, URL, or docker:// image",
            path.display(),
            index + 1
        ))),
    }
}

/// Parse an `e = ["KEY=VALUE", …]` array into pairs (values kept raw — `$VAR`
/// is expanded at launch, not here).
fn parse_env(
    value: Option<&toml::Value>,
    member: &str,
    path: &Path,
) -> Result<Vec<(String, String)>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let list = value.as_array().ok_or_else(|| {
        Error::Manifest(format!(
            "{}: member `{member}`: `e` must be an array of \"KEY=VALUE\" strings",
            path.display()
        ))
    })?;
    let mut out = Vec::new();
    for item in list {
        let s = item.as_str().ok_or_else(|| {
            Error::Manifest(format!(
                "{}: member `{member}`: every `e` entry must be a \"KEY=VALUE\" string",
                path.display()
            ))
        })?;
        let Some((k, v)) = s.split_once('=') else {
            return Err(Error::Manifest(format!(
                "{}: member `{member}`: `e` entry `{s}` is not KEY=VALUE",
                path.display()
            )));
        };
        if k.is_empty() {
            return Err(Error::Manifest(format!(
                "{}: member `{member}`: `e` entry `{s}` has an empty key",
                path.display()
            )));
        }
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

/// Parse a `params = { key = "value", … }` table into pairs (values kept
/// raw — `$VAR` expands at launch like `env`; `{param}`/`{app.param}` holes
/// resolve when the stack wires this member's params namespace).
fn parse_params(
    value: Option<&toml::Value>,
    member: &str,
    path: &Path,
) -> Result<Vec<(String, String)>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let table = value.as_table().ok_or_else(|| {
        Error::Manifest(format!(
            "{}: member `{member}`: `params` must be a table of strings, e.g. params = {{ password = \"$PW\" }}",
            path.display()
        ))
    })?;
    let mut out = Vec::new();
    for (k, v) in table {
        let s = v.as_str().ok_or_else(|| {
            Error::Manifest(format!(
                "{}: member `{member}`: `params.{k}` must be a string",
                path.display()
            ))
        })?;
        out.push((k.clone(), s.to_string()));
    }
    Ok(out)
}

/// A field that is a single string or an array of strings → `Vec<String>`.
fn string_list(
    value: Option<&toml::Value>,
    field: &str,
    member: &str,
    path: &Path,
) -> Result<Vec<String>> {
    match value {
        None => Ok(Vec::new()),
        Some(toml::Value::String(one)) => Ok(vec![one.clone()]),
        Some(toml::Value::Array(list)) => {
            let mut out = Vec::new();
            for item in list {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => {
                        return Err(Error::Manifest(format!(
                            "{}: member `{member}`: `{field}` must be strings",
                            path.display()
                        )))
                    }
                }
            }
            Ok(out)
        }
        Some(_) => Err(Error::Manifest(format!(
            "{}: member `{member}`: `{field}` must be a string or an array of strings",
            path.display()
        ))),
    }
}

/// Substitute `$VAR` / `${VAR}` in a value from `lookup`. `$$` is a literal
/// `$`. An undefined variable is a hard error — never a silent empty value.
/// `who` labels the error (e.g. `member `db` env POSTGRES_PASSWORD`).
pub fn expand_vars(
    input: &str,
    who: &str,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('$') => {
                chars.next();
                out.push('$');
            }
            Some('{') => {
                chars.next();
                let mut name = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    name.push(c);
                }
                if !closed {
                    return Err(Error::Manifest(format!(
                        "{who}: unterminated `${{{name}` (missing `}}`)"
                    )));
                }
                out.push_str(&resolve_var(&name, who, lookup)?);
            }
            Some(c) if c.is_ascii_alphabetic() || *c == '_' => {
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        name.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push_str(&resolve_var(&name, who, lookup)?);
            }
            // a `$` not starting a variable stays literal
            _ => out.push('$'),
        }
    }
    Ok(out)
}

fn resolve_var(name: &str, who: &str, lookup: &impl Fn(&str) -> Option<String>) -> Result<String> {
    lookup(name).ok_or_else(|| {
        Error::Manifest(format!(
            "{who}: `${name}` is not set — provide it in the environment or an --env-file"
        ))
    })
}

/// Expand every `$VAR` in one of a member's string lists — `publish` or
/// `domain`. These are holes for the same reason a password is: a stack
/// published for other people to run cannot know the hostname of the host
/// that will run it, nor which ports are already spoken for there. Without
/// this, a shared stack has to hard-code somebody else's domain.
///
/// The catalog keeps these RAW: what is published is the template, holes
/// intact, and filling them is the deployer's business.
pub fn expand_member_list(
    values: &[String],
    member: &str,
    what: &str,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<Vec<String>> {
    values
        .iter()
        .map(|v| expand_vars(v, &format!("member `{member}` {what}"), lookup))
        .collect()
}

/// Expand every `$VAR` in a member's env, returning launch-ready pairs.
pub fn expand_member_env(
    member: &Member,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<Vec<(String, String)>> {
    member
        .env
        .iter()
        .map(|(k, v)| {
            let who = format!("member `{}` env {k}", member.name);
            Ok((k.clone(), expand_vars(v, &who, lookup)?))
        })
        .collect()
}

/// Expand every `$VAR` in a member's params, returning launch-ready pairs.
/// Mirrors `expand_member_env`: only `$VAR` is handled here — `{param}` /
/// `{app.param}` holes are left for the stack's params-namespace resolution.
pub fn expand_member_params(
    member: &Member,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, String>> {
    member
        .params
        .iter()
        .map(|(k, v)| {
            let who = format!("member `{}` params {k}", member.name);
            Ok((k.clone(), expand_vars(v, &who, lookup)?))
        })
        .collect()
}

/// A stack member's manifest, read from wherever its `run =` kind keeps one:
/// `Manifest::load(dir/ply.toml)` for a local `run = "./dir"` member (`dir`
/// is `Some`, and this is the SOURCE manifest — not yet built, the same file
/// the child's own build step reads); the embedded `/.manifest.toml` read
/// straight out of an image file for a registry `run =` member (`dir` is
/// `None`, `target` is the already-fetched image path) OR a local
/// `run = "./app.img"` member (`MemberSource::Path` covers both a directory
/// AND an `.img` file — see its doc comment — so `dir` can be `Some` and
/// still name a file, not a directory).
///
/// Dispatch is on what `dir` (when given) actually IS on disk, not on the
/// source's discriminant: a directory reads `ply.toml`; a file — `.img` or
/// otherwise — is handed to the same embedded-manifest reader the `None`
/// arm uses.
///
/// Under `ply up` there is no manifest available for a `run = "https://…"`
/// member before the child fetches it — callers must not call this for
/// those; treat them as declaring no `[params]` (empty decls, no
/// self-config) instead. A host has already fetched every member's image
/// by the time it reconciles, so there it reads like any other.
pub fn member_manifest(target: &str, dir: Option<&Path>) -> Result<Manifest> {
    match dir {
        Some(dir) if dir.is_file() => crate::image::read::read_manifest(dir),
        Some(dir) => Manifest::load(&dir.join("ply.toml")),
        None => crate::image::read::read_manifest(Path::new(target)),
    }
}

/// The member names this member's `env`/`publish`/`domain` templates
/// reference via `{app.param}` — an implicit `after` edge for each, unioned
/// into ordering and cycle detection alongside the explicit `after` list.
/// `{self.x}` and a ref naming this member's own name are not edges — a
/// member always "has" itself, that is not a dependency.
pub fn derived_after(member: &Member) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let values = member
        .env
        .iter()
        .map(|(_, v)| v)
        .chain(member.publish.iter())
        .chain(member.domain.iter());
    for v in values {
        for r in params::refs(v) {
            let PRef { app, .. } = r;
            let Some(app) = app else { continue };
            if app == member.name || app == "self" {
                continue;
            }
            if seen.insert(app.clone()) {
                out.push(app);
            }
        }
    }
    out
}

/// Kahn's algorithm, declaration order as the tie-break; cycles are errors.
fn topo_sort(members: Vec<Member>, path: &Path) -> Result<Vec<Member>> {
    // effective edges: the explicit `after` list — each entry parsed down to
    // its `wait.app` (`member`, `member.param == '…'` and friends all order
    // on the member they name, whatever else they also check) — unioned
    // with the edges implied by `{app.param}` refs in env/publish/domain
    // (`derived_after`). Every `m.after` entry already parsed successfully
    // in `parse`'s validation pass above, so a parse failure here can't
    // happen; falling back to the raw string is just defensive.
    let edges: Vec<BTreeSet<String>> = members
        .iter()
        .map(|m| {
            let mut e: BTreeSet<String> = m
                .after
                .iter()
                .map(|dep| {
                    crate::runtime::after::parse_wait(dep)
                        .map(|w| w.app)
                        .unwrap_or_else(|_| dep.clone())
                })
                .collect();
            e.extend(derived_after(m));
            e
        })
        .collect();
    let mut indegree = vec![0usize; members.len()];
    for (i, edge) in edges.iter().enumerate() {
        indegree[i] = edge.len();
    }
    let mut order: Vec<usize> = Vec::with_capacity(members.len());
    let mut ready: Vec<usize> = (0..members.len()).filter(|i| indegree[*i] == 0).collect();
    while let Some(next) = ready.first().copied() {
        ready.remove(0);
        order.push(next);
        for (i, edge) in edges.iter().enumerate() {
            if edge.contains(&members[next].name) {
                indegree[i] -= 1;
                if indegree[i] == 0 {
                    ready.push(i);
                    ready.sort(); // declaration order among newly-ready
                }
            }
        }
    }
    if order.len() != members.len() {
        let stuck: Vec<&str> = members
            .iter()
            .enumerate()
            .filter(|(i, _)| !order.contains(i))
            .map(|(_, m)| m.name.as_str())
            .collect();
        return Err(Error::Manifest(format!(
            "{}: [[app]] has an `after` cycle involving {}",
            path.display(),
            stuck.join(", ")
        )));
    }
    let mut by_index: Vec<(usize, Member)> = members.into_iter().enumerate().collect();
    by_index.sort_by_key(|(i, _)| order.iter().position(|o| o == i).unwrap_or(usize::MAX));
    Ok(by_index.into_iter().map(|(_, m)| m).collect())
}

/// The members to start for `ply up [names…]`: the named ones plus their
/// transitive `after` closure, in stack order. Empty selection = everything.
pub fn select<'a>(stack: &'a Stack, names: &[String]) -> Result<Vec<&'a Member>> {
    if names.is_empty() {
        return Ok(stack.members.iter().collect());
    }
    let known: BTreeSet<&str> = stack.members.iter().map(|m| m.name.as_str()).collect();
    for name in names {
        if !known.contains(name.as_str()) {
            return Err(Error::Manifest(format!(
                "`{name}` is not a member (members: {})",
                stack
                    .members
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let mut wanted: BTreeSet<String> = names.iter().cloned().collect();
    loop {
        let mut grew = false;
        for member in &stack.members {
            if wanted.contains(&member.name) {
                for dep in &member.after {
                    // `after` entries are conditions (`member`,
                    // `member.param == '…'`, …), not bare member names —
                    // pull the app back out, same as topo_sort. Every entry
                    // already parsed successfully in `parse`'s validation
                    // pass, so the fallback to the raw string is defensive.
                    let app = crate::runtime::after::parse_wait(dep)
                        .map(|w| w.app)
                        .unwrap_or_else(|_| dep.clone());
                    grew |= wanted.insert(app);
                }
                // A `{app.param}` reference IS an edge — the same union
                // `topo_sort` orders on and reconcile's `member_edges`
                // converges on. Without it `ply up server` on a stack whose
                // server reads `{db.url}` selects `server` alone and dies
                // resolving `db`.
                for app in derived_after(member) {
                    grew |= wanted.insert(app);
                }
            }
        }
        if !grew {
            break;
        }
    }
    Ok(stack
        .members
        .iter()
        .filter(|m| wanted.contains(&m.name))
        .collect())
}

// --- the stack lock ----------------------------------------------------------

/// One `run =` member's pin: the reference as written, the version it
/// resolved to, and the image digest per arch (each arch's image is a
/// different artifact of the same version — the lock is shared across
/// machines, so digests are keyed by arch).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pin {
    pub reference: String,
    pub version: String,
    /// arch ("x64"/"arm64") → sha256 digest of the app image
    pub digests: BTreeMap<String, String>,
}

/// `ply.lock` in a stack dir: resolved `run =` members. A digest hit means
/// `ply up` starts straight from the store — no index fetch, no download.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StackLock {
    pub pins: BTreeMap<String, Pin>,
}

impl StackLock {
    pub fn load(dir: &Path) -> StackLock {
        let mut pins = BTreeMap::new();
        let Ok(text) = std::fs::read_to_string(dir.join("ply.lock")) else {
            return StackLock { pins };
        };
        let Ok(doc) = text.parse::<toml::Value>() else {
            return StackLock { pins };
        };
        if let Some(stack) = doc.get("stack").and_then(|s| s.as_table()) {
            for (name, entry) in stack {
                let (Some(reference), Some(version)) = (
                    entry.get("ref").and_then(|v| v.as_str()),
                    entry.get("version").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                let mut digests = BTreeMap::new();
                if let Some(table) = entry.get("digest").and_then(|v| v.as_table()) {
                    for (arch, digest) in table {
                        if let Some(d) = digest.as_str() {
                            digests.insert(arch.clone(), d.to_string());
                        }
                    }
                }
                pins.insert(
                    name.clone(),
                    Pin {
                        reference: reference.to_string(),
                        version: version.to_string(),
                        digests,
                    },
                );
            }
        }
        StackLock { pins }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let mut text = String::from(
            "# stack lock — resolved `run =` member versions; `ply up --refresh` re-resolves\n",
        );
        for (name, pin) in &self.pins {
            text.push_str(&format!(
                "[stack.{name}]\nref = \"{}\"\nversion = \"{}\"\n",
                pin.reference, pin.version
            ));
            for (arch, digest) in &pin.digests {
                text.push_str(&format!("digest.{arch} = \"{digest}\"\n"));
            }
            text.push('\n');
        }
        let path = dir.join("ply.lock");
        std::fs::write(&path, text).map_err(|source| Error::Io { path, source })
    }

    /// The member's pin, honored only while the manifest reference is
    /// unchanged (editing `run =` re-resolves).
    pub fn pinned(&self, member: &str, reference: &str) -> Option<&Pin> {
        self.pins.get(member).filter(|p| p.reference == reference)
    }

    pub fn record(
        &mut self,
        member: &str,
        reference: &str,
        version: &str,
        arch: &str,
        digest: &str,
    ) {
        let pin = self.pins.entry(member.to_string()).or_default();
        if pin.reference != reference || pin.version != version {
            pin.digests.clear();
        }
        pin.reference = reference.to_string();
        pin.version = version.to_string();
        pin.digests.insert(arch.to_string(), digest.to_string());
    }
}

// --- resolving a stack: params, env, waits ----------------------------------
//
// One resolver, two callers. `ply up` spawns a `ply run` child per member;
// `ply reconcile` writes a systemd unit per member. Both build the same
// per-member inputs and read the same `Resolution` back, so a stack means
// the same thing on a laptop and on a host — same `{}` resolution, same
// secret taint, same ordering.

/// The container-side port of a member's FIRST `--publish` entry — the
/// `{port}` fact: the segment after the last `:`, so
/// `"internal:5433:5432"` -> `5432`, `"internal:5432"` -> `5432`,
/// `"3000"` -> `3000`. No publish entry -> `None`, and `{port}`/`{addr}`/
/// `{base_url}` then name that gap instead of guessing.
pub fn container_port(publish: &[String]) -> Option<u16> {
    publish.first()?.rsplit(':').next()?.parse().ok()
}

/// The `{port}` fact when nothing is published: the app's own `[ports]`
/// entry, if it declares exactly one. The spec scopes `{port}`/`{addr}`/
/// `{base_url}` to "apps with a published/labeled port" — a keg that labels
/// one port HAS said where it listens, and a stack member (or a bare
/// `ply run`) that adds no `--publish` should still read `{port}` off that
/// label rather than name a gap. Two or more labelled ports is a genuine
/// ambiguity, and stays `None`: `{port}` then names the gap as before.
pub fn manifest_port(manifest: &Manifest) -> Option<u16> {
    match manifest.ports.len() {
        1 => manifest.ports.values().next().copied(),
        _ => None,
    }
}

/// One member's inputs to [`resolve_stack`]: its wiring with `$VAR` already
/// expanded and `{}` holes still raw, its manifest, and the built-in facts
/// only the caller knows (the version that will actually run, the published
/// port, `--scale`, the image identity).
pub struct MemberInput {
    /// The member identity — its `--name`, its `<name>.ply` host, and the
    /// handle other members' `{name.param}` refs and `after` entries use.
    pub name: String,
    /// The member's manifest, or `None` when there is none to read yet (a
    /// `run = "https://…"` member under `ply up`, whose image the child
    /// fetches): treated as declaring no `[params]` — empty decls, no
    /// self-config, and a stack `params =` override on it is an error.
    pub manifest: Option<Manifest>,
    /// The member's stack `e = [...]` env; `{}` holes raw.
    pub env: Vec<(String, String)>,
    /// `params = {...}` overrides (`{}` holes stay raw — [`params::namespace`]
    /// resolves those against this member's own namespace).
    pub params: BTreeMap<String, String>,
    /// The member's explicit `after = [...]`, raw: a plain member name or a
    /// condition (`db.finish_boot == 'ok'`), verbatim either way.
    pub after: Vec<String>,
    /// `--publish` entries — scanned for `{app.param}` refs, each a derived
    /// edge.
    pub publish: Vec<String>,
    /// `--domain` entries — scanned like `publish`.
    pub domain: Vec<String>,
    /// The `{version}` fact: the version of what will actually run.
    pub version: Option<String>,
    /// The `{port}` fact: the container-side port of the member's FIRST
    /// publish entry. `None` = it publishes nothing, and the resolver falls
    /// back to the manifest's own single `[ports]` entry (see
    /// [`manifest_port`]) before `{port}`/`{addr}`/`{base_url}` name the gap
    /// rather than resolve to a guess.
    pub port: Option<u16>,
    /// The `{scale}` fact — `None` means 1.
    pub scale: Option<u32>,
    /// The `{image}` fact: the store digest of an already-fetched image, a
    /// URL, or `None` for a local dir whose image doesn't exist yet.
    pub image: Option<String>,
}

/// One resolved env var for a stack member, ready to become a `ply run` `-e`
/// (or, on a host, a line in the member's 0600 env file).
#[derive(Clone, PartialEq)]
pub struct ResolvedEnv {
    pub key: String,
    pub value: String,
    /// Taint from [`params::Resolved`] — never printed, and delivered to the
    /// child out of argv: `ply up` sets it on the child's environment and
    /// passes a bare `-e KEY`; `ply reconcile` writes it to an env file
    /// instead of the world-readable unit.
    pub secret: bool,
    pub source: EnvSource,
}

/// Hand-written (not derived): a secret's `value` must never come out of a
/// `{:?}` — a stray `dbg!`, or an `anyhow` context on a type that embeds
/// this — the way a derive would print it verbatim. `PartialEq` (derived,
/// above) still compares the real value; only this rendering masks it.
impl std::fmt::Debug for ResolvedEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value: &dyn std::fmt::Debug = if self.secret {
            &"********"
        } else {
            &self.value
        };
        f.debug_struct("ResolvedEnv")
            .field("key", &self.key)
            .field("value", value)
            .field("secret", &self.secret)
            .field("source", &self.source)
            .finish()
    }
}

/// Where a [`ResolvedEnv`]'s value came from — `Display` renders `ply up
/// --plan`'s per-entry "source" column.
#[derive(Debug, Clone, PartialEq)]
pub enum EnvSource {
    /// A stack `e = [...]` entry with no `{}` holes — today's behavior,
    /// unchanged.
    StackE,
    /// A stack `e = [...]` entry whose value contained one or more `{}`
    /// holes; the string is the rendered ref(s), e.g. `"{db.url}"`.
    ParamRef(String),
    /// Provider self-config: a hole in the member's OWN `[env]`, resolved
    /// against its own namespace — a multi-hole or literal-plus-hole value,
    /// or a single hole whose param is neither a stack override nor a
    /// secret decl (`Override`/`Minted` below cover those finer cases).
    SelfEnv,
    /// Provider self-config whose single hole named a param the stack's
    /// `params = {...}` overrode.
    Override,
    /// Provider self-config whose single hole named a declared secret param
    /// — the string is that secret's file, as [`SecretStore::label`] names
    /// it (e.g. `"secrets/db.password"`), shown so a reader can go straight
    /// to it.
    Minted(String),
}

impl std::fmt::Display for EnvSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvSource::StackE => write!(f, "stack e"),
            EnvSource::ParamRef(r) => write!(f, "{r}"),
            EnvSource::SelfEnv => write!(f, "manifest [env]"),
            EnvSource::Override => write!(f, "params (stack override)"),
            EnvSource::Minted(path) => write!(f, "minted  {path}"),
        }
    }
}

/// The result of resolving every member's params namespace and env:
/// per-member resolved param tables, per-member launch env (with secret
/// taint and provenance), and per-member wait lists (`after` ∪
/// `derived_after`).
#[derive(Default)]
pub struct Resolution {
    /// Every member's fully-resolved param table. Neither caller needs it
    /// to launch (env delivery and waits are enough) — self-config
    /// attribution and secret collection both happen from a local table
    /// inside [`resolve_stack`], before this struct is built — but it is
    /// what a params dump would read, and the resolver's own tests assert
    /// on it.
    pub namespaces: BTreeMap<String, params::Namespace>,
    pub env: BTreeMap<String, Vec<ResolvedEnv>>,
    pub waits: BTreeMap<String, Vec<String>>,
    /// The LEAF secrets across every member: the resolved value of every
    /// param whose declaration is `ParamDecl::Secret{..}` (stack-overridden
    /// or external counts too), minus empty strings and the literal
    /// `"(will mint)"`. NOT every `secret: true` value in `namespaces` — a
    /// *computed* param (e.g. `url`) is tainted by a leaf it embeds, but
    /// isn't itself a leaf, so `--plan` masks the leaf substring inside it
    /// rather than the whole composed value. `ply up --plan`'s only reader.
    pub secret_values: BTreeSet<String>,
}

/// Hand-written (not derived): `env` and a namespace's resolved values mask
/// through `Resolved`'s and `ResolvedEnv`'s own `Debug` impls, but
/// `secret_values` is a bare `BTreeSet<String>` of raw secret substrings —
/// a derived `Debug` would print it verbatim. A `{:?}` of a `Resolution`
/// must never leak a secret.
///
/// A namespace's CAPTURED failures ([`params::Namespace`]'s `Err` slots) are
/// messages, not values, and every one this crate builds names params and
/// members only — except a malformed-template message, which quotes the
/// offending source, and a stack `params =` override's source can be an
/// operator-supplied secret. It reaches the operator who wrote it either
/// way (that is the error they get), but a `{:?}` is not how it should
/// travel, so error slots print as `<unresolved>` here.
impl std::fmt::Debug for Resolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let namespaces: BTreeMap<&str, BTreeMap<&str, std::result::Result<&Resolved, &str>>> = self
            .namespaces
            .iter()
            .map(|(member, ns)| {
                let ns = ns
                    .iter()
                    .map(|(name, slot)| (name.as_str(), slot.as_ref().map_err(|_| "<unresolved>")))
                    .collect();
                (member.as_str(), ns)
            })
            .collect();
        f.debug_struct("Resolution")
            .field("namespaces", &namespaces)
            .field("env", &self.env)
            .field("waits", &self.waits)
            .field(
                "secret_values",
                &format_args!("<{} redacted>", self.secret_values.len()),
            )
            .finish()
    }
}

/// Resolve a whole stack: per member, build its [`MemberFacts`] and resolve
/// its params namespace (minting/loading secrets through `secrets`);
/// interpolate `{}` holes in its stack `e = [...]` env against every
/// member's namespace; and — for members whose own manifest declares
/// `[params]` and has hole-y `[env]` — resolve those holes against the
/// member's own namespace as provider self-config, delivered as `-e`
/// overrides. Waits come out as `after` ∪ derived edges.
///
/// `secrets` is `None` only when the caller has nowhere to keep secret
/// files (`ply up` with no stack dir); a member that needs one then fails,
/// naming the gap.
///
/// `plan_only`: a missing (non-external) secret resolves to the literal
/// `"(will mint)"` (tainted, no file write) instead of minting; a missing
/// EXTERNAL secret still errors — plan is the validator, not an escape
/// hatch. Always `false` on the host path, which converges rather than
/// previews.
///
/// Additive: a member with no `{}` refs, no `params =` overrides and no
/// manifest `[params]` resolves to entries byte-for-byte identical (value,
/// order) to the pairs it came in with, each tagged [`EnvSource::StackE`].
///
/// Thin wrapper over [`resolve_impl`] with `host_available: true` — every
/// existing caller (`ply up`'s netns, a host's rootful `/etc/hosts`) can
/// always resolve `<member>.ply`. See [`resolve_stack_for_run`] for the one
/// caller where that isn't a given.
pub fn resolve_stack(
    members: &[MemberInput],
    secrets: Option<&SecretStore>,
    plan_only: bool,
) -> Result<Resolution> {
    resolve_impl(members, secrets, plan_only, true)
}

/// The standalone-`ply run` counterpart to [`resolve_stack`]: one member, no
/// stack around it. `secrets` is always `None` — a bare `ply run` has
/// nowhere durable to keep one — and `plan_only` is always `true`, so a
/// declared secret resolves to the tainted `"(will mint)"` placeholder
/// rather than erroring or minting (see `resolve_impl`'s `mint` closure);
/// `runtime::run::resolve_manifest_env` turns that taint into a hard error
/// unless the operator's own `-e` already covers the key.
///
/// `host_available`: unlike a stack member, a standalone run cannot always
/// assume `<name>.ply` resolves — only a rootful run gets the `/etc/hosts`
/// entry that makes it real (see `runtime::run::run`'s `rootless`).
pub(crate) fn resolve_stack_for_run(
    member: &MemberInput,
    host_available: bool,
) -> Result<Resolution> {
    resolve_impl(std::slice::from_ref(member), None, true, host_available)
}

/// Shared body of [`resolve_stack`] and [`resolve_stack_for_run`] — see
/// [`resolve_stack`]'s doc for what this does. `host_available` says
/// whether `{host}`/`{addr}`/`{base_url}` can resolve at all: always `true`
/// through `resolve_stack`, only conditionally so through
/// `resolve_stack_for_run`.
fn resolve_impl(
    members: &[MemberInput],
    secrets: Option<&SecretStore>,
    plan_only: bool,
    host_available: bool,
) -> Result<Resolution> {
    struct Info {
        decls: BTreeMap<String, params::ParamDecl>,
        facts: MemberFacts,
    }

    // Pass 1: read every member's declared params, validate `params =`
    // overrides against that set, and build its built-in facts.
    let mut infos: BTreeMap<String, Info> = BTreeMap::new();
    for m in members {
        let decls = m
            .manifest
            .as_ref()
            .map(|man| man.param_decls())
            .transpose()?
            .unwrap_or_default();

        for key in m.params.keys() {
            if decls.contains_key(key) {
                continue;
            }
            if !m.manifest.as_ref().is_some_and(|man| man.params.is_some()) {
                return Err(Error::Manifest(format!(
                    "member `{}`: sets params.{key}, but {} declares no params — \
                     add `{key}` to its manifest's [params], or drop `params.{key}` from the stack",
                    m.name, m.name
                )));
            }
            let mut declared: Vec<&str> = decls.keys().map(String::as_str).collect();
            declared.sort_unstable();
            return Err(Error::Manifest(format!(
                "member `{}`: params.{key} is not a declared param — declared: {}",
                m.name,
                declared.join(", ")
            )));
        }

        let facts = MemberFacts {
            name: m.name.clone(),
            version: m.version.clone(),
            // `<member>.ply` resolves wherever a stack runs: `ply up`'s
            // netns wires it as a loopback alias, and rootful `ply run` —
            // the host's mode — writes it into `/etc/hosts` alongside the
            // bridge address. So it is always available under every
            // existing caller (`resolve_stack`'s `host_available: true`).
            // The exception is `resolve_stack_for_run`'s rootless case: a
            // bare `ply run` with no stack netns and no `/etc/hosts` entry,
            // where `<name>.ply` genuinely resolves nowhere.
            host: host_available.then(|| format!("{}.ply", m.name)),
            // Nothing published: fall back to the app's own single labelled
            // port, so a keg's `url = "…{port}…"` still resolves for a
            // member (or a bare `ply run`) that adds no `--publish`.
            port: m
                .port
                .or_else(|| m.manifest.as_ref().and_then(manifest_port)),
            scale: m.scale.unwrap_or(1),
            arch: crate::image::name::Arch::host().as_str().to_string(),
            image: m.image.clone(),
        };
        infos.insert(m.name.clone(), Info { decls, facts });
    }

    // Pass 2: resolve every member's own params namespace. Stack order is
    // topo-sorted (producers before consumers), but namespace resolution
    // itself never crosses members — only the env interpolation below does
    // — so build order here doesn't need to follow the graph.
    let mut namespaces: BTreeMap<String, params::Namespace> = BTreeMap::new();
    let mut secret_values: BTreeSet<String> = BTreeSet::new();
    for m in members {
        let info = &infos[&m.name];
        let member = m.name.clone();
        let mut mint = |param: &str| -> Result<String> {
            let external = matches!(
                info.decls.get(param),
                Some(params::ParamDecl::Secret { external: true })
            );
            if plan_only {
                // A plan never mints or writes, so a missing store (no stack
                // dir, or no stack at all — `resolve_stack_for_run` always
                // passes `None`) is the same "not found yet" case as an
                // empty one: an external secret still must be provided by
                // hand; anything else previews as the tainted placeholder it
                // will become.
                let existing = match secrets {
                    Some(ss) => ss.get(&member, param)?,
                    None => None,
                };
                return match existing {
                    Some(v) => Ok(v),
                    None if external => Err(Error::Runtime(format!(
                        "secret {member}.{param} is external — provide it: ply secret set {member}.{param}"
                    ))),
                    None => Ok("(will mint)".to_string()),
                };
            }
            let Some(ss) = secrets else {
                return Err(Error::Runtime(format!(
                    "member `{member}` needs secret `{param}` but this stack has no directory \
                     to store secrets in — run `ply up -C <dir>`"
                )));
            };
            ss.load_or_mint(&member, param, external)
        };
        let ns = params::namespace(&info.facts, &info.decls, &m.params, &mut mint)
            .map_err(|e| Error::Manifest(format!("member `{}`: resolving params: {e}", m.name)))?;
        // LEAF secrets only: a declared secret's own resolved value, never a
        // computed param that merely embeds one (`ns[name].secret` would be
        // true for both — `decls` is what tells them apart).
        // A param that CAPTURED a resolution failure (see
        // `params::namespace`) has no value to collect and is simply
        // absent here — a secret never can, they resolve eagerly.
        for (name, decl) in &info.decls {
            if matches!(decl, params::ParamDecl::Secret { .. }) {
                if let Some(v) = ns
                    .get(name)
                    .and_then(|r| r.as_ref().ok())
                    .map(|r| r.value.as_str())
                {
                    if !v.is_empty() && v != "(will mint)" {
                        secret_values.insert(v.to_string());
                    }
                }
            }
        }
        namespaces.insert(m.name.clone(), ns);
    }

    // Pass 3: interpolate each member's stack env, add provider self-config,
    // and compute waits.
    let mut env: BTreeMap<String, Vec<ResolvedEnv>> = BTreeMap::new();
    let mut waits: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for m in members {
        let member = m.name.clone();
        let info = &infos[&member];
        let mut entries: Vec<ResolvedEnv> = Vec::new();

        // Provider self-config: only when this member declares [params] AND
        // has hole-y [env] values. Delivered FIRST so an explicit stack
        // `e =` entry for the same key (pushed below) still wins — explicit
        // beats automatic.
        if let Some(man) = m.manifest.as_ref().filter(|man| man.params.is_some()) {
            for (key, raw) in &man.env {
                let who = format!("member `{member}` [env] {key}");
                let pieces = params::parse_template(raw, &who)?;
                if !pieces.iter().any(|pc| matches!(pc, Piece::Hole(_))) {
                    continue; // no hole: already correct baked into the image
                }
                let own_ns = &namespaces[&member];
                let resolved = params::interpolate(&pieces, &mut |pref: &PRef| {
                    params::lookup(own_ns, pref, &member).cloned()
                })?;
                // Finer attribution for `--plan`: a single bare hole names
                // exactly one param, so it can be traced to why it has that
                // value — a stack override, a minted secret, or (the
                // catch-all, including multi-hole/literal-plus-hole values)
                // the manifest's own [env].
                let source = match pieces.as_slice() {
                    [Piece::Hole(pref)] if m.params.contains_key(&pref.param) => {
                        EnvSource::Override
                    }
                    [Piece::Hole(pref)]
                        if matches!(
                            info.decls.get(&pref.param),
                            Some(params::ParamDecl::Secret { .. })
                        ) =>
                    {
                        let path = match secrets {
                            Some(ss) => ss.label(&member, &pref.param),
                            // unreachable in practice: resolving this secret
                            // in Pass 2 would already have errored without a
                            // store to mint/load it from.
                            None => format!("secrets/{member}.{}", pref.param),
                        };
                        EnvSource::Minted(path)
                    }
                    _ => EnvSource::SelfEnv,
                };
                entries.push(ResolvedEnv {
                    key: key.clone(),
                    value: resolved.value,
                    secret: resolved.secret,
                    source,
                });
            }
        }

        // The member's own stack `e = [...]` entries: `$VAR` is already
        // expanded (unchanged, upstream); resolve `{}` holes now, against
        // the whole stack's namespaces — LIVE names are already rejected at
        // stack-parse time, so this is belt-and-braces.
        let mut edges: BTreeSet<String> = BTreeSet::new();
        for (key, raw) in &m.env {
            let who = format!("member `{member}` env {key}");
            let pieces = params::parse_template(raw, &who)?;
            let refs = params::refs(raw);
            let resolved = params::interpolate(&pieces, &mut |pref: &PRef| {
                let owner = match pref.app.as_deref() {
                    Some(a) if a != "self" => a,
                    _ => member.as_str(),
                };
                let ns = namespaces
                    .get(owner)
                    .ok_or_else(|| Error::Manifest(format!("{who}: no such member `{owner}`")))?;
                params::lookup(ns, pref, owner).cloned()
            })?;
            // Several holes in one value each get their own edge (below),
            // but the label only names ONE ref — the first, in written
            // order — for `--plan`'s single `via {ref}` annotation; it is
            // not a rendering of the whole value.
            let source = match refs.first() {
                None => EnvSource::StackE,
                Some(r) => {
                    let rendered = match &r.app {
                        Some(a) => format!("{{{a}.{}}}", r.param),
                        None => format!("{{{}}}", r.param),
                    };
                    EnvSource::ParamRef(rendered)
                }
            };
            for r in &refs {
                if let Some(app) = &r.app {
                    if app != &member && app != "self" {
                        edges.insert(app.clone());
                    }
                }
            }
            entries.push(ResolvedEnv {
                key: key.clone(),
                value: resolved.value,
                secret: resolved.secret,
                source,
            });
        }
        // `publish`/`domain` refs derive edges too (mirrors
        // [`derived_after`]), even though their values aren't interpolated
        // here.
        for v in m.publish.iter().chain(m.domain.iter()) {
            for r in params::refs(v) {
                if let Some(app) = r.app {
                    if app != member && app != "self" {
                        edges.insert(app);
                    }
                }
            }
        }

        let mut w = m.after.clone();
        for dep in edges {
            if !w.contains(&dep) {
                w.push(dep);
            }
        }
        waits.insert(member.clone(), w);
        env.insert(member, entries);
    }

    Ok(Resolution {
        namespaces,
        env,
        waits,
        secret_values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `run = "docker://…"` is the fourth member kind: imported at `up` time
    /// through the same cache `ply run docker://` uses, then treated like an
    /// `.img` member. Its default name is the image's, without registry,
    /// path, tag or digest.
    #[test]
    fn a_docker_member_classifies_and_names_itself_after_the_image() {
        let path = Path::new("ply.toml");
        let (source, name) = classify_run("docker://redis:7-alpine", path, 0).unwrap();
        assert_eq!(
            source,
            MemberSource::Docker("docker://redis:7-alpine".into())
        );
        assert_eq!(name.as_deref(), Some("redis"));
        assert_eq!(
            docker_member_name("docker://ghcr.io/org/app:1.2").as_deref(),
            Some("app")
        );
        assert_eq!(
            docker_member_name("docker://library/nginx").as_deref(),
            Some("nginx")
        );
        assert_eq!(
            docker_member_name("docker://redis@sha256:abc123").as_deref(),
            Some("redis")
        );
        assert_eq!(docker_member_name("docker://"), None);
        let err = classify_run("dockr://redis", path, 2)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("docker://"),
            "the error names every accepted form: {err}"
        );
    }

    fn stack_of(text: &str) -> Stack {
        parse(text, Path::new("ply.toml")).unwrap().unwrap()
    }

    fn stack_err(text: &str) -> String {
        parse(text, Path::new("ply.toml")).unwrap_err().to_string()
    }

    const LAB: &str = r#"
[stack]
name = "lab"

[[app]]
run  = "postgres@17"
name = "db"
e    = ["POSTGRES_PASSWORD=dev", "PGPORT=5442"]
volume = ["/var/lib/postgresql/data"]

[[app]]
run   = "./server"
after = "db"
publish = ["internal:8080"]

[[app]]
run   = "./web"
after = ["server"]
scale = 2
"#;

    /// `app = "name"` (a string) is the registry DEPLOYMENT lane — the
    /// classifier must pass it through, not die on "must be an array".
    /// Regression: 0.1.50 broke every registry-lane deployment on a host.
    #[test]
    fn plain_app_key_is_not_a_stack() {
        let spec = "app = \"dashboard\"\ngrant_links = true\npublish = [\"internal:7070\"]\n";
        assert!(parse(spec, Path::new("dashboard.toml")).unwrap().is_none());
    }

    #[test]
    fn parses_and_orders_the_lab_stack() {
        let stack = stack_of(LAB);
        assert_eq!(stack.name.as_deref(), Some("lab"));
        let names: Vec<&str> = stack.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["db", "server", "web"]);
        let db = &stack.members[0];
        assert_eq!(
            db.source,
            MemberSource::Run {
                name: "postgres".into(),
                version: Some("17".into())
            }
        );
        assert!(db.env.contains(&("PGPORT".into(), "5442".into())));
        assert_eq!(db.volume, vec!["/var/lib/postgresql/data"]);
        // ./server default name is its dir basename; after coerces string→[…]
        assert_eq!(stack.members[1].name, "server");
        assert_eq!(stack.members[1].after, vec!["db"]);
        assert_eq!(stack.members[1].publish, vec!["internal:8080"]);
        assert_eq!(stack.members[2].scale, Some(2));
    }

    #[test]
    fn stack_header_accepts_owner() {
        let s = parse(
            "[stack]\nname = \"todos\"\nowner = \"iluxav\"\nversion = \"0.1.0\"\n[[app]]\nrun = \"postgres@17\"\n",
            Path::new("stack.toml"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(s.owner.as_deref(), Some("iluxav"));
    }

    #[test]
    fn no_app_array_is_none() {
        assert!(parse("[package]\nname = \"x\"\n", Path::new("p"))
            .unwrap()
            .is_none());
        assert!(parse("", Path::new("p")).unwrap().is_none());
    }

    #[test]
    fn rejects_package_plus_app() {
        let err = parse(
            "[package]\nname = \"x\"\n[[app]]\nrun = \"redis\"\n",
            Path::new("p"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("pure wiring"));
    }

    #[test]
    fn default_name_from_registry_ref() {
        let s = stack_of("[[app]]\nrun = \"redis@8\"\n");
        assert_eq!(s.members[0].name, "redis");
    }

    #[test]
    fn duplicate_names_rejected() {
        let err = parse(
            "[[app]]\nrun = \"postgres@17\"\n[[app]]\nrun = \"postgres@17\"\n",
            Path::new("p"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("two members named"));
    }

    #[test]
    fn rejects_bad_members() {
        for (toml, msg) in [
            ("[[app]]\nname = \"x\"\n", "needs `run"),
            ("[[app]]\nrun = \"redis\"\nscale = 0\n", "positive integer"),
            ("[[app]]\nrun = \"redis\"\ntypo = 1\n", "unknown key"),
            ("[[app]]\nrun = \"Not A Ref\"\n", "not a package reference"),
            (
                "[[app]]\nrun = \"redis\"\ne = [\"NOEQ\"]\n",
                "not KEY=VALUE",
            ),
            (
                "[[app]]\nrun = \"redis\"\nname=\"a\"\nafter = \"ghost\"\n",
                "not a member",
            ),
            (
                "[[app]]\nrun = \"redis\"\nname=\"a\"\nafter = \"a\"\n",
                "waits for itself",
            ),
        ] {
            let err = parse(toml, Path::new("p")).unwrap_err().to_string();
            assert!(
                err.contains(msg),
                "`{toml}` should mention `{msg}`, got: {err}"
            );
        }
    }

    #[test]
    fn a_member_egress_override_parses_its_three_spellings() {
        let text = "[[app]]\nrun = \"postgres@17\"\nname = \"db\"\negress = { mode = \"enforce\" }\n\n[[app]]\nrun = \"redis@8\"\nname = \"cache\"\negress = { mode = \"audit\", allow = [\"*.stripe.com\", \"1.1.1.1\"] }\n\n[[app]]\nrun = \"nginx@1\"\nname = \"edge\"\negress = \"off\"\n";
        let stack = parse(text, Path::new("stack.toml")).unwrap().unwrap();
        let by_name = |n: &str| {
            stack
                .members
                .iter()
                .find(|m| m.name == n)
                .unwrap()
                .egress
                .clone()
                .unwrap()
        };
        assert_eq!(
            by_name("db"),
            crate::egress::EgressOverride {
                mode: Some(crate::egress::Mode::Enforce),
                allow: None
            }
        );
        let cache = by_name("cache");
        assert_eq!(cache.mode, Some(crate::egress::Mode::Audit));
        assert_eq!(cache.allow.unwrap().len(), 2);
        assert_eq!(
            by_name("edge"),
            crate::egress::EgressOverride {
                mode: Some(crate::egress::Mode::Off),
                allow: None
            }
        );
    }

    #[test]
    fn a_bad_member_egress_names_the_member() {
        let text =
            "[[app]]\nrun = \"postgres@17\"\nname = \"db\"\negress = { mode = \"strict\" }\n";
        let err = parse(text, Path::new("stack.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("member `db`"), "{err}");
        assert!(err.contains("expected off, audit or enforce"), "{err}");
        let text = "[[app]]\nrun = \"postgres@17\"\nname = \"db\"\negress = 5\n";
        let err = parse(text, Path::new("stack.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`egress`"), "{err}");
    }

    #[test]
    fn rejects_cycles() {
        let err = parse(
            "[[app]]\nrun = \"x\"\nname=\"a\"\nafter = \"b\"\n[[app]]\nrun = \"y\"\nname=\"b\"\nafter = \"a\"\n",
            Path::new("p"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("cycle"));
    }

    #[test]
    fn condition_after_orders_on_its_app_and_keeps_the_raw_condition() {
        let stack = stack_of(
            "[[app]]\nrun = \"redis\"\nname = \"a\"\n\
             [[app]]\nrun = \"redis\"\nname = \"b\"\nafter = [\"a.finish_boot == 'ok'\"]\n",
        );
        let names: Vec<&str> = stack.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"], "b waits on a, so a orders first");
        assert_eq!(stack.members[1].name, "b");
        assert_eq!(
            stack.members[1].after,
            vec!["a.finish_boot == 'ok'"],
            "the raw condition rides through to `ply up`'s --after, not just `a`"
        );
    }

    #[test]
    fn malformed_after_condition_is_rejected_at_stack_parse() {
        let err = stack_err(
            "[[app]]\nrun = \"redis\"\nname = \"a\"\n\
             [[app]]\nrun = \"redis\"\nname = \"b\"\nafter = [\"a.finish_boot != 'ok'\"]\n",
        );
        assert!(
            err.contains("expected APP, APP.PARAM, or APP.PARAM == 'value'"),
            "{err}"
        );
    }

    #[test]
    fn selection_pulls_dependencies() {
        let stack = stack_of(LAB);
        let only_web: Vec<&str> = select(&stack, &["web".into()])
            .unwrap()
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(
            only_web,
            vec!["db", "server", "web"],
            "after closure comes along"
        );
        let only_db: Vec<&str> = select(&stack, &["db".into()])
            .unwrap()
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(only_db, vec!["db"]);
        assert!(select(&stack, &["ghost".into()]).is_err());
    }

    #[test]
    fn selection_pulls_in_a_conditional_dependency() {
        let stack = stack_of(
            "[[app]]\nrun = \"redis\"\nname = \"a\"\n\
             [[app]]\nrun = \"redis\"\nname = \"b\"\nafter = [\"a.finish_boot == 'ok'\"]\n",
        );
        let only_b: Vec<&str> = select(&stack, &["b".into()])
            .unwrap()
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(
            only_b,
            vec!["a", "b"],
            "`ply up b` must pull in `a`, not drop it because `after` names a \
             condition rather than a bare member"
        );
    }

    /// I3: the mirror of the test above for a DERIVED edge. `topo_sort` and
    /// reconcile's `member_edges` both union `derived_after` in; `select`
    /// must too, or `ply up server` on the spec's own stack selects only
    /// `server` and dies naming `db` as "no such member".
    #[test]
    fn selection_pulls_in_a_derived_dependency() {
        let stack = stack_of(
            "[[app]]\nrun = \"postgres@17\"\nname = \"db\"\n\
             [[app]]\nrun = \"./server\"\nname = \"server\"\ne = [\"DATABASE_URL={db.url}\"]\n",
        );
        let only_server: Vec<&str> = select(&stack, &["server".into()])
            .unwrap()
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(
            only_server,
            vec!["db", "server"],
            "a `{{db.url}}` reference IS the edge — selection must follow it"
        );
    }

    /// I5: `{}` interpolation is a v1 feature of `e =` values and `params =`
    /// overrides only; `publish`/`domain` reach `parse_publish` raw, so a
    /// hole there would have become a literal `{db.port}` in a port spec.
    /// Reject it at parse with the limitation and the remedy.
    #[test]
    fn a_hole_in_publish_or_domain_is_rejected_at_parse() {
        let e = stack_err(
            "[[app]]\nrun = \"postgres@17\"\nname = \"db\"\n\
             [[app]]\nrun = \"./server\"\nname = \"server\"\npublish = \"{db.port}:3000\"\n",
        );
        assert!(e.contains("publish"), "{e}");
        assert!(e.contains("write the port literally"), "{e}");

        let e = stack_err("[[app]]\nrun = \"./web\"\nname = \"web\"\ndomain = \"{web.host}\"\n");
        assert!(e.contains("domain"), "{e}");
    }

    // --- $VAR expansion ------------------------------------------------------

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn stack_label_on_an_app_spec_is_not_a_stack_reference() {
        // bare name is a valid reference (resolves against the default source)
        assert_eq!(parse_ref("stack = \"todos\"\n").unwrap().reference, "todos");

        // the same key on a single-app spec is a cosmetic GROUPING label.
        // Before this, such a file was hijacked into the stack-ref lane and
        // the app never deployed.
        assert_eq!(parse_ref("from = \"redis@8\"\nstack = \"plybox\"\n"), None);
        assert_eq!(
            parse_ref("repo = \"https://github.com/o/a\"\nstack = \"plybox\"\n"),
            None
        );

        // and — the case a denylist could not catch — a MISSPELLED source
        // key. `form =` is not a reference either; it goes to Spec::parse,
        // which names the typo instead of tearing the app down.
        assert_eq!(parse_ref("form = \"redis@8\"\nstack = \"plybox\"\n"), None);
        assert_eq!(
            parse_ref("stack = \"plybox\"\npublish = [\"3000\"]\n"),
            None
        );
    }

    #[test]
    fn member_env_accepts_env_and_e_but_not_both() {
        let stack = parse(
            r#"
            [[app]]
            run  = "postgres@17"
            name = "db"
            env  = ["POSTGRES_PASSWORD=dev"]

            [[app]]
            run  = "redis@7"
            name = "cache"
            e    = ["MAXMEM=64mb"]
            "#,
            std::path::Path::new("stack.toml"),
        )
        .unwrap()
        .unwrap();
        let by = |n: &str| {
            stack
                .members
                .iter()
                .find(|m| m.name == n)
                .unwrap()
                .env
                .clone()
        };
        assert_eq!(by("db"), vec![("POSTGRES_PASSWORD".into(), "dev".into())]);
        assert_eq!(by("cache"), vec![("MAXMEM".into(), "64mb".into())]);

        // both spellings on one member is a mistake, not a silent winner
        let err = parse(
            r#"
            [[app]]
            run  = "redis@7"
            name = "cache"
            env  = ["A=1"]
            e    = ["A=2"]
            "#,
            std::path::Path::new("stack.toml"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("both `env` and `e`"), "{err}");
    }

    #[test]
    fn expand_basic_and_braced() {
        let e = env(&[("PW", "s3cret"), ("HOST", "db.ply")]);
        assert_eq!(expand_vars("p=$PW", "w", &e).unwrap(), "p=s3cret");
        assert_eq!(
            expand_vars("postgres://u:${PW}@${HOST}:5432", "w", &e).unwrap(),
            "postgres://u:s3cret@db.ply:5432"
        );
    }

    #[test]
    fn expand_literal_dollar_and_trailing() {
        let e = env(&[]);
        assert_eq!(expand_vars("cost is $$5", "w", &e).unwrap(), "cost is $5");
        assert_eq!(expand_vars("ends with $", "w", &e).unwrap(), "ends with $");
        assert_eq!(
            expand_vars("price $ each", "w", &e).unwrap(),
            "price $ each"
        );
    }

    #[test]
    fn expand_undefined_is_an_error() {
        let e = env(&[]);
        let err = expand_vars("p=$MISSING", "member `db` env P", &e)
            .unwrap_err()
            .to_string();
        assert!(err.contains("$MISSING` is not set"), "{err}");
        assert!(err.contains("member `db`"), "labels the source: {err}");
    }

    #[test]
    fn expand_member_env_maps_all() {
        let m = Member {
            name: "db".into(),
            source: MemberSource::Run {
                name: "postgres".into(),
                version: None,
            },
            env: vec![("A".into(), "$X".into()), ("B".into(), "plain".into())],
            params: vec![],
            after: vec![],
            publish: vec![],
            volume: vec![],
            domain: vec![],
            scale: None,
            egress: None,
        };
        let e = env(&[("X", "1")]);
        let out = expand_member_env(&m, &e).unwrap();
        assert_eq!(
            out,
            vec![("A".into(), "1".into()), ("B".into(), "plain".into())]
        );
    }

    #[test]
    fn lock_roundtrip_and_pin_invalidation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = StackLock::default();
        lock.record("db", "postgres@17", "17.10.0", "x64", "sha256:abc");
        lock.record("db", "postgres@17", "17.10.0", "arm64", "sha256:def");
        lock.save(tmp.path()).unwrap();
        let loaded = StackLock::load(tmp.path());
        assert_eq!(loaded, lock);
        let pin = loaded.pinned("db", "postgres@17").unwrap();
        assert_eq!(pin.version, "17.10.0");
        assert_eq!(pin.digests.get("x64"), Some(&"sha256:abc".to_string()));
        assert_eq!(pin.digests.get("arm64"), Some(&"sha256:def".to_string()));
        assert!(loaded.pinned("db", "postgres@18").is_none());
        assert!(loaded.pinned("other", "postgres@17").is_none());
    }

    /// A namespaced member follows a published app: the ref keeps its
    /// namespace (that is where the catalog lives), while the member's
    /// identity on the host is the bare package name.
    /// The overlay is how one committed stack serves both worlds: production
    /// values in stack.toml, local truths (loopback, a port that dodges what
    /// this machine runs) in stack.dev.toml. Env MERGES by key so an overlay
    /// adds DATABASE_URL without discarding the member's other vars.
    #[test]
    fn dev_overlay_overrides_members() {
        let dir = std::env::temp_dir().join(format!("ply-overlay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stack_file = dir.join("stack.toml");
        std::fs::write(
            &stack_file,
            "[stack]\nname = \"todos\"\n\n[[app]]\nrun = \"postgres@17\"\nname = \"db\"\npublish = [\"internal:5432\"]\ne = [\"POSTGRES_DB=todos\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("stack.dev.toml"),
            "[[app]]\nname = \"db\"\npublish = [\"internal:5433\"]\ne = [\"DATABASE_URL=postgres://localhost:5433/todos\"]\n",
        )
        .unwrap();

        let (mut stack, file) = discover(&dir).unwrap().expect("stack");
        let what = apply_dev_overlay(&mut stack, &file)
            .unwrap()
            .expect("overlay applied");
        assert!(what.contains("db("), "summary names the member: {what}");

        let db = &stack.members[0];
        assert_eq!(db.publish, vec!["internal:5433"], "publish replaced");
        let env: std::collections::BTreeMap<_, _> = db.env.iter().cloned().collect();
        assert_eq!(
            env.get("POSTGRES_DB").map(String::as_str),
            Some("todos"),
            "kept"
        );
        assert!(env.contains_key("DATABASE_URL"), "added");

        // a host reads the stack file alone — the overlay is `ply up` only
        let text = std::fs::read_to_string(&stack_file).unwrap();
        let host_view = parse(&text, &stack_file).unwrap().unwrap();
        assert_eq!(host_view.members[0].publish, vec!["internal:5432"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Overriding a member that does not exist is a typo, and a silent no-op
    /// would be the worst possible answer.
    #[test]
    fn dev_overlay_rejects_unknown_member() {
        let dir = std::env::temp_dir().join(format!("ply-overlay-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stack_file = dir.join("stack.toml");
        std::fs::write(
            &stack_file,
            "[[app]]\nrun = \"redis@8\"\nname = \"cache\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("stack.dev.toml"),
            "[[app]]\nname = \"cach\"\nscale = 2\n",
        )
        .unwrap();

        let (mut stack, file) = discover(&dir).unwrap().unwrap();
        let err = apply_dev_overlay(&mut stack, &file)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no member named `cach`"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `ply up` finds stack.toml; "is this dir a stack?" still answers
    /// from ply.toml, so an app repo that also ships a stack.toml stays
    /// runnable with `ply run ./dir`.
    #[test]
    fn discover_prefers_stack_toml_but_load_does_not() {
        let dir = std::env::temp_dir().join(format!("ply-stack-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ply.toml"),
            "[package]\nname = \"web\"\nversion = \"0.1.0\"\nentrypoint = [\"node\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("stack.toml"),
            "[stack]\nname = \"todos\"\n\n[[app]]\nrun = \"postgres@17\"\n",
        )
        .unwrap();

        let (found, from) = discover(&dir).unwrap().expect("stack.toml is the stack");
        assert_eq!(found.name.as_deref(), Some("todos"));
        assert!(from.ends_with("stack.toml"), "reports which file it read");
        assert!(
            load(&dir).unwrap().is_none(),
            "an app dir stays an app even when it ships a stack.toml"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn namespaced_member_ref() {
        let stack = stack_of("[[app]]\nrun = \"ply/ply-web\"\npublish = [\"internal:3000\"]\n");
        let m = &stack.members[0];
        assert_eq!(m.name, "ply-web");
        assert_eq!(
            m.source,
            MemberSource::Run {
                name: "ply/ply-web".into(),
                version: None,
            }
        );
    }

    #[test]
    fn lock_record_clears_digests_on_version_change() {
        let mut lock = StackLock::default();
        lock.record("db", "postgres@17", "17.10.0", "x64", "sha256:abc");
        lock.record("db", "postgres@17", "17.11.0", "arm64", "sha256:def");
        let pin = lock.pinned("db", "postgres@17").unwrap();
        assert_eq!(pin.version, "17.11.0");
        assert!(
            !pin.digests.contains_key("x64"),
            "stale arch digest dropped"
        );
    }

    // --- params -----------------------------------------------------------

    #[test]
    fn member_params_parse_and_stay_raw() {
        let s = stack_of(
            r#"
[[app]]
run = "postgres@17"
name = "db"
params = { database = "todos", password = "$PW" }
"#,
        );
        let m = &s.members[0];
        assert!(m.params.contains(&("password".into(), "$PW".into())));
        let x = expand_member_params(m, &|k| (k == "PW").then(|| "s".into())).unwrap();
        assert_eq!(x["password"], "s");
    }

    #[test]
    fn env_refs_derive_the_edge_and_order() {
        let s = stack_of(
            r#"
[[app]]
run = "./server"
e = ["DATABASE_URL={db.url}"]
[[app]]
run = "postgres@17"
name = "db"
"#,
        );
        assert_eq!(
            s.members.last().unwrap().name,
            "server",
            "db ordered first via the ref"
        );
    }

    #[test]
    fn ref_to_unknown_member_is_an_error() {
        let e = stack_err(
            r#"
[[app]]
run = "./server"
e = ["X={ghost.url}"]
"#,
        );
        assert!(e.contains("no stack member named `ghost`"), "{e}");
    }

    #[test]
    fn live_param_in_env_is_rejected_with_the_wait_hint() {
        let e = stack_err(
            r#"
[[app]]
run = "./server"
e = ["S={db.state}"]
[[app]]
run = "postgres@17"
name = "db"
"#,
        );
        assert!(e.contains("is live"), "{e}");
    }

    // --- member_manifest ----------------------------------------------------

    #[test]
    fn member_manifest_reads_a_local_dirs_ply_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ply.toml"),
            "[package]\nname = \"server\"\nversion = \"1.0.0\"\nentrypoint = [\"node\"]\n",
        )
        .unwrap();
        let m = member_manifest(&dir.path().display().to_string(), Some(dir.path())).unwrap();
        assert_eq!(m.package.name, "server");
    }

    #[test]
    fn member_manifest_reads_the_embedded_manifest_of_an_image() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("pkg.img");
        crate::image::squashfs::write_image(
            &[],
            &[crate::image::squashfs::ExtraFile {
                path: "/.manifest.toml".into(),
                bytes: b"[package]\nname = \"db\"\nversion = \"17.0.0\"\n".to_vec(),
                mode: 0o444,
            }],
            &image,
        )
        .unwrap();
        let m = member_manifest(&image.display().to_string(), None).unwrap();
        assert_eq!(m.package.name, "db");
        assert_eq!(m.package.version.to_string(), "17.0.0");
    }

    /// `run = "./app.img"` parses to `MemberSource::Path` exactly like a
    /// directory does (see its doc comment) — `dir` arrives `Some`, but
    /// naming a FILE, not a directory. `member_manifest` must still read
    /// the embedded manifest, not try `<img>/ply.toml`.
    #[test]
    fn member_manifest_reads_an_img_file_passed_as_a_path_member() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("app.img");
        crate::image::squashfs::write_image(
            &[],
            &[crate::image::squashfs::ExtraFile {
                path: "/.manifest.toml".into(),
                bytes: b"[package]\nname = \"app\"\nversion = \"2.0.0\"\n".to_vec(),
                mode: 0o444,
            }],
            &image,
        )
        .unwrap();
        let m = member_manifest(&image.display().to_string(), Some(&image)).unwrap();
        assert_eq!(m.package.name, "app");
        assert_eq!(m.package.version.to_string(), "2.0.0");
    }
}

#[cfg(test)]
mod stack_ref_tests {
    use super::*;

    /// A deployment file that NAMES a published stack.
    #[test]
    fn a_reference_file_yields_the_reference() {
        let r = parse_ref("stack = \"iluxav/todos\"\n").unwrap();
        assert_eq!(r.reference, "iluxav/todos");
        assert!(r.auto && r.env_file.is_none());
        // a reference may carry the deployer's env_file and a pin — keys the
        // old string-returning parser silently dropped
        let r =
            parse_ref("stack = \"iluxav/todos\"\nenv_file = \".env/todos.env\"\nauto = false\n")
                .unwrap();
        assert_eq!(r.env_file.as_deref(), Some(".env/todos.env"));
        assert!(!r.auto);
    }

    /// A real stack file spells its members out and carries a `[stack]`
    /// TABLE. That must never be mistaken for a reference — the two shapes
    /// share a key name and nothing else.
    #[test]
    fn a_spelled_out_stack_is_not_a_reference() {
        let text = "[stack]\nname = \"todos\"\nversion = \"0.1.0\"\n\n[[app]]\nrun = \"postgres@17\"\nname = \"db\"\n";
        assert_eq!(parse_ref(text), None);
        assert!(
            parse(text, std::path::Path::new("todos.toml"))
                .unwrap()
                .is_some(),
            "the spelled-out lane must still parse as a stack"
        );
    }

    /// A single-app deployment is untouched by the new lane.
    #[test]
    fn a_single_app_spec_is_not_a_reference() {
        assert_eq!(parse_ref("app = \"redis\"\nversion = \"8\"\n"), None);
    }

    /// The dispatch reaches the reference lane only because a reference file
    /// has no `[[app]]` blocks — so `parse` must decline it.
    #[test]
    fn parse_declines_a_reference_file_so_the_dispatch_falls_through() {
        assert!(
            parse("stack = \"iluxav/todos\"\n", std::path::Path::new("t.toml"))
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(test)]
mod member_hole_tests {
    use super::*;

    fn member(publish: &[&str], domain: &[&str]) -> Member {
        Member {
            name: "web".into(),
            source: MemberSource::Run {
                name: "web".into(),
                version: None,
            },
            env: vec![],
            params: vec![],
            after: vec![],
            publish: publish.iter().map(|s| s.to_string()).collect(),
            volume: vec![],
            domain: domain.iter().map(|s| s.to_string()).collect(),
            scale: None,
            egress: None,
        }
    }

    /// A published stack cannot know the hostname of whoever deploys it, so
    /// `domain` has to be a hole like any secret. Without this a shared stack
    /// must hard-code someone else's domain.
    #[test]
    fn a_domain_hole_is_filled() {
        let m = member(&[], &["$SITE"]);
        let lookup = |k: &str| (k == "SITE").then(|| "todos.plybox.sh".to_string());
        let got = expand_member_list(&m.domain, &m.name, "domain", &lookup).unwrap();
        assert_eq!(got, vec!["todos.plybox.sh"]);
    }

    /// Ports collide differently on every host, so `publish` is a hole too.
    #[test]
    fn a_publish_hole_is_filled() {
        let m = member(&["internal:$PORT"], &[]);
        let lookup = |k: &str| (k == "PORT").then(|| "3000".to_string());
        let got = expand_member_list(&m.publish, &m.name, "publish", &lookup).unwrap();
        assert_eq!(got, vec!["internal:3000"]);
    }

    /// An unset hole must fail loudly and name where it was, exactly as an
    /// unset env hole does — a blank domain would silently serve nowhere.
    #[test]
    fn an_unset_hole_names_the_member_and_the_field() {
        let m = member(&[], &["$SITE"]);
        let err = expand_member_list(&m.domain, &m.name, "domain", &|_: &str| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("web"), "{err}");
        assert!(err.contains("domain"), "{err}");
    }

    /// Untouched values pass through — the common case must not change.
    #[test]
    fn a_plain_value_is_unchanged() {
        let m = member(&["internal:3000"], &["todos.example.com"]);
        let lookup = |_: &str| None;
        assert_eq!(
            expand_member_list(&m.publish, &m.name, "publish", &lookup).unwrap(),
            vec!["internal:3000"]
        );
        assert_eq!(
            expand_member_list(&m.domain, &m.name, "domain", &lookup).unwrap(),
            vec!["todos.example.com"]
        );
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use crate::secrets::SecretStore;

    const DB_MANIFEST: &str = r#"
[package]
name = "db"
version = "17.0.0"
entrypoint = ["postgres"]

[params]
database = "todos"
password = { secret = true }
url = "postgres://postgres:{password}@{host}:{port}/{database}"

[env]
POSTGRES_DB = "{database}"
POSTGRES_PASSWORD = "{password}"
"#;

    const PLAIN_MANIFEST: &str = r#"
[package]
name = "server"
version = "1.0.0"
entrypoint = ["node", "server.js"]
"#;

    /// A `MemberInput` with everything but the fields a test cares about
    /// defaulted the way a bare `[[app]]` block would arrive.
    fn input(name: &str, manifest: Option<&str>) -> MemberInput {
        let manifest = manifest.map(|t| Manifest::parse(t).unwrap());
        MemberInput {
            name: name.to_string(),
            version: manifest.as_ref().map(|m| m.package.version.to_string()),
            manifest,
            env: Vec::new(),
            params: BTreeMap::new(),
            after: Vec::new(),
            publish: Vec::new(),
            domain: Vec::new(),
            port: None,
            scale: None,
            image: None,
        }
    }

    fn env_of<'a>(res: &'a Resolution, member: &str, key: &str) -> &'a ResolvedEnv {
        res.env[member]
            .iter()
            .find(|e| e.key == key)
            .unwrap_or_else(|| panic!("no {key} in {member}'s resolved env: {:?}", res.env[member]))
    }

    #[test]
    fn container_port_takes_the_segment_after_the_last_colon() {
        assert_eq!(container_port(&["internal:5433:5432".into()]), Some(5432));
        assert_eq!(container_port(&["internal:5432".into()]), Some(5432));
        assert_eq!(container_port(&["3000".into()]), Some(3000));
        assert_eq!(container_port(&[]), None);
        assert_eq!(container_port(&["internal:notaport".into()]), None);
    }

    #[test]
    fn cross_member_ref_resolves_taints_and_derives_the_wait() {
        let root = tempfile::tempdir().unwrap();
        let secrets = SecretStore::for_stack(root.path());

        let mut db = input("db", Some(DB_MANIFEST));
        db.publish = vec!["internal:5432".to_string()];
        db.port = Some(5432);
        let mut server = input("server", Some(PLAIN_MANIFEST));
        server.env = vec![("DATABASE_URL".to_string(), "{db.url}".to_string())];

        let resolution = resolve_stack(&[db, server], Some(&secrets), false).unwrap();

        let secret = secrets
            .get("db", "password")
            .unwrap()
            .expect("minted and persisted");
        assert_eq!(
            secret.len(),
            32,
            "the minted secret shape from secrets::mint"
        );

        let pg_db = env_of(&resolution, "db", "POSTGRES_DB");
        assert_eq!(pg_db.value, "todos");
        assert!(!pg_db.secret);
        assert_eq!(pg_db.source, EnvSource::SelfEnv);

        let pg_pw = env_of(&resolution, "db", "POSTGRES_PASSWORD");
        assert_eq!(pg_pw.value, secret);
        assert!(pg_pw.secret);

        let url = env_of(&resolution, "server", "DATABASE_URL");
        assert_eq!(
            url.value,
            format!("postgres://postgres:{secret}@db.ply:5432/todos")
        );
        assert!(url.secret, "a value built from a secret hole stays tainted");
        assert_eq!(url.source, EnvSource::ParamRef("{db.url}".to_string()));

        // no explicit `after` on server, but the {db.url} ref is itself the
        // wait — derived, not declared.
        assert_eq!(resolution.waits["server"], vec!["db".to_string()]);
        assert!(resolution.waits["db"].is_empty());
    }

    #[test]
    fn a_member_with_no_params_or_holes_resolves_byte_for_byte_unchanged() {
        let mut web = input("web", Some(PLAIN_MANIFEST));
        web.env = vec![
            ("A".to_string(), "plain".to_string()),
            ("B".to_string(), "also plain".to_string()),
        ];
        let original_env = web.env.clone();

        let resolution = resolve_stack(&[web], None, false).unwrap();

        // `Resolution.env` is the single source of truth both callers read
        // — additive: no {} holes, no params, so every entry is the
        // original (key, value) pair, in the original order, tagged
        // `StackE`, never secret.
        let resolved: Vec<(String, String)> = resolution.env["web"]
            .iter()
            .map(|e| (e.key.clone(), e.value.clone()))
            .collect();
        assert_eq!(resolved, original_env);
        for e in &resolution.env["web"] {
            assert_eq!(e.source, EnvSource::StackE);
            assert!(!e.secret);
        }
    }

    const SECRET_ONLY_MANIFEST: &str = r#"
[package]
name = "db"
version = "17.0.0"
entrypoint = ["postgres"]

[params]
password = { secret = true }

[env]
POSTGRES_PASSWORD = "{password}"
"#;

    #[test]
    fn plan_only_renders_will_mint_for_a_missing_mintable_secret_without_writing() {
        let root = tempfile::tempdir().unwrap();
        let secrets = SecretStore::for_stack(root.path());

        let db = input("db", Some(SECRET_ONLY_MANIFEST));
        let resolution = resolve_stack(&[db], Some(&secrets), true).unwrap();

        let pg_pw = env_of(&resolution, "db", "POSTGRES_PASSWORD");
        assert_eq!(pg_pw.value, "(will mint)");
        assert!(pg_pw.secret);

        assert_eq!(
            secrets.get("db", "password").unwrap(),
            None,
            "plan_only must never write the secret file"
        );
    }

    #[test]
    fn plan_only_still_refuses_a_missing_external_secret() {
        let root = tempfile::tempdir().unwrap();
        let secrets = SecretStore::for_stack(root.path());
        let api = input(
            "api",
            Some(
                r#"
[package]
name = "api"
version = "1.0.0"
entrypoint = ["api"]

[params]
stripe_key = { secret = true, external = true }
"#,
            ),
        );
        let e = resolve_stack(&[api], Some(&secrets), true)
            .unwrap_err()
            .to_string();
        assert!(e.contains("external"), "{e}");
        assert!(e.contains("ply secret set api.stripe_key"), "{e}");
    }

    /// `ply up <stack> --plan` (or `ply up some.toml --plan`) with no lock
    /// dir reaches `resolve_stack(_, None, true)` for real — a preview mints
    /// nothing, so it must not require a directory to store secrets in
    /// either. Locks in the `mint` closure's `plan_only` branch: no store at
    /// all previews the same way an empty store does (mintable → tainted
    /// `(will mint)`, no file written anywhere; external → still refused).
    /// The non-plan path is untouched — still a hard error naming the gap.
    #[test]
    fn plan_only_without_a_secret_store_previews_mintable_secrets_and_still_refuses_external_ones()
    {
        let db = input("db", Some(SECRET_ONLY_MANIFEST));
        let resolution = resolve_stack(&[db], None, true).unwrap();

        let pg_pw = env_of(&resolution, "db", "POSTGRES_PASSWORD");
        assert_eq!(pg_pw.value, "(will mint)");
        assert!(pg_pw.secret);

        let api = input(
            "api",
            Some(
                r#"
[package]
name = "api"
version = "1.0.0"
entrypoint = ["api"]

[params]
stripe_key = { secret = true, external = true }
"#,
            ),
        );
        let e = resolve_stack(&[api], None, true).unwrap_err().to_string();
        assert!(e.contains("external"), "{e}");
        assert!(e.contains("ply secret set api.stripe_key"), "{e}");

        // Not `--plan`: still the original hard error, no directory to mint
        // into or persist a secret file in.
        let db = input("db", Some(SECRET_ONLY_MANIFEST));
        let e = resolve_stack(&[db], None, false).unwrap_err().to_string();
        assert!(e.contains("no directory to store secrets in"), "{e}");
    }

    #[test]
    fn several_refs_in_one_value_label_the_source_with_the_first_only() {
        let db = input(
            "db",
            Some(
                r#"
[package]
name = "db"
version = "17.0.0"
entrypoint = ["postgres"]

[params]
user = "postgres"
database = "todos"
"#,
            ),
        );
        let mut server = input("server", Some(PLAIN_MANIFEST));
        server.env = vec![("X".to_string(), "{db.user}-{db.database}".to_string())];

        let resolution = resolve_stack(&[db, server], None, false).unwrap();

        let x = env_of(&resolution, "server", "X");
        assert_eq!(x.value, "postgres-todos");
        assert_eq!(
            x.source,
            EnvSource::ParamRef("{db.user}".to_string()),
            "several holes in one value label the source with the FIRST ref only"
        );
        // both refs still each contribute to the (deduplicated) wait list.
        assert_eq!(resolution.waits["server"], vec!["db".to_string()]);
    }

    #[test]
    fn a_member_without_a_manifest_has_no_params_and_no_self_config() {
        let resolution = resolve_stack(&[input("ext", None)], None, false).unwrap();
        assert!(resolution.env["ext"].is_empty());
        assert!(resolution.waits["ext"].is_empty());
    }

    #[test]
    fn a_params_override_on_a_member_without_a_manifest_is_an_error() {
        let mut ext = input("ext", None);
        ext.params = BTreeMap::from([("foo".to_string(), "bar".to_string())]);
        let e = resolve_stack(&[ext], None, false).unwrap_err().to_string();
        assert!(e.contains("declares no params"), "{e}");
    }

    /// `<member>.ply` resolves wherever a stack runs (`ply up`'s netns
    /// loopback alias; the host's rootful `/etc/hosts` bridge entry) — so
    /// `{host}` is available unconditionally, whether or not the member
    /// publishes a port at all.
    #[test]
    fn host_fact_is_always_present() {
        let db = input(
            "db",
            Some(
                r#"
[package]
name = "db"
version = "17.0.0"
entrypoint = ["postgres"]

[params]
hostname = "{host}"
"#,
            ),
        );
        // no publish entry at all — host still resolves.
        let resolution = resolve_stack(&[db], None, false).unwrap();
        assert_eq!(
            resolution.namespaces["db"]["hostname"]
                .as_ref()
                .unwrap()
                .value,
            "db.ply"
        );
    }

    /// `port`/`addr`/`base_url` are NOT unconditional like `host` — they
    /// still need a published port (the caller passes `port: None` without
    /// one), and `{port}`/`{addr}`/`{base_url}` name that gap the same way
    /// they always have.
    #[test]
    fn port_ref_without_a_publish_entry_still_names_the_gap() {
        let db = input(
            "db",
            Some(
                r#"
[package]
name = "db"
version = "17.0.0"
entrypoint = ["postgres"]

[params]
addr_ref = "{addr}"

[env]
ADDR = "{addr_ref}"
"#,
            ),
        );
        let e = resolve_stack(&[db], None, false).unwrap_err().to_string();
        assert!(e.contains("publishes no port"), "{e}");
    }

    /// C1: the same param, UNREFERENCED, must not fail the stack — the keg
    /// `[params]` tables ship a computed `url` reading `{host}`/`{port}`,
    /// and a member of either keg with no `publish` would otherwise fail
    /// `ply up`/`--plan`/`reconcile` before anything even asked for `url`.
    #[test]
    fn an_unreferenced_computed_param_that_reads_an_absent_fact_resolves_the_rest() {
        let db = input(
            "db",
            Some(
                r#"
[package]
name = "db"
version = "17.0.0"
entrypoint = ["postgres"]

[params]
database = "todos"
url = "postgres://x@{host}:{port}/{database}"

[env]
POSTGRES_DB = "{database}"
"#,
            ),
        );
        // no publish entry, no [ports]: `{port}` has no value, but nothing
        // reads `url`, so the rest of the namespace resolves.
        let resolution = resolve_stack(&[db], None, false).unwrap();
        assert_eq!(env_of(&resolution, "db", "POSTGRES_DB").value, "todos");
    }

    /// C1(b): with nothing published, the `{port}` fact falls back to the
    /// manifest's own single `[ports]` entry — "only for apps with a
    /// published/labeled port", per the spec.
    #[test]
    fn the_port_fact_falls_back_to_a_single_declared_port() {
        let db = input(
            "db",
            Some(
                r#"
[package]
name = "db"
version = "17.0.0"
entrypoint = ["postgres"]

[ports]
db = 5432

[params]
url = "postgres://x@{host}:{port}/todos"

[env]
DB_URL = "{url}"
"#,
            ),
        );
        let resolution = resolve_stack(&[db], None, false).unwrap();
        assert_eq!(
            env_of(&resolution, "db", "DB_URL").value,
            "postgres://x@db.ply:5432/todos"
        );
    }

    #[test]
    fn self_config_attribution_distinguishes_override_minted_and_manifest_env() {
        let root = tempfile::tempdir().unwrap();
        let secrets = SecretStore::for_stack(root.path());
        let mut db = input(
            "db",
            Some(
                r#"
[package]
name = "db"
version = "17.0.0"
entrypoint = ["postgres"]

[params]
database = "todos"
region = "us-east"
password = { secret = true }

[env]
POSTGRES_DB = "{database}"
POSTGRES_REGION = "{region}"
POSTGRES_PASSWORD = "{password}"
"#,
            ),
        );
        db.params = BTreeMap::from([("database".to_string(), "prod_todos".to_string())]);

        let resolution = resolve_stack(&[db], Some(&secrets), false).unwrap();

        assert_eq!(
            env_of(&resolution, "db", "POSTGRES_DB").source,
            EnvSource::Override,
            "a param the stack overrode is tagged Override"
        );
        assert_eq!(
            env_of(&resolution, "db", "POSTGRES_REGION").source,
            EnvSource::SelfEnv,
            "a plain manifest default, not overridden, stays SelfEnv"
        );
        match &env_of(&resolution, "db", "POSTGRES_PASSWORD").source {
            EnvSource::Minted(path) => assert_eq!(path, "secrets/db.password"),
            other => panic!("expected Minted, got {other:?}"),
        }
    }

    // --- Debug masking --------------------------------------------------------

    #[test]
    fn secret_env_entries_mask_their_value_in_debug_output() {
        let entry = ResolvedEnv {
            key: "PASSWORD".to_string(),
            value: "s3cr3t-value".to_string(),
            secret: true,
            source: EnvSource::SelfEnv,
        };
        let debug = format!("{entry:?}");
        assert!(debug.contains("********"), "{debug}");
        assert!(!debug.contains("s3cr3t-value"), "{debug}");

        // a non-secret entry prints its value verbatim, unmasked.
        let plain = ResolvedEnv {
            key: "NODE_ENV".to_string(),
            value: "production".to_string(),
            secret: false,
            source: EnvSource::StackE,
        };
        assert!(format!("{plain:?}").contains("production"));
    }

    #[test]
    fn a_resolutions_debug_output_never_leaks_a_leaf_secret() {
        // `namespaces`/`env` mask through `Resolved`/`ResolvedEnv`'s own
        // `Debug`; this test is about `secret_values` specifically — a bare
        // `BTreeSet<String>` that a derived `Debug` would print verbatim —
        // and about a namespace's CAPTURED failure messages, which a
        // derived `Debug` would likewise print as the raw strings they are.
        let mut namespaces = BTreeMap::new();
        namespaces.insert(
            "db".to_string(),
            BTreeMap::from([
                (
                    "password".to_string(),
                    Ok(Resolved {
                        value: "s3cr3t-leaf".to_string(),
                        secret: true,
                    }),
                ),
                (
                    "url".to_string(),
                    Err("db.url: stray `{` in `s3cr3t-leaf{`".to_string()),
                ),
            ]),
        );
        let resolution = Resolution {
            namespaces,
            secret_values: BTreeSet::from(["s3cr3t-leaf".to_string()]),
            ..Default::default()
        };
        let debug = format!("{resolution:?}");
        assert!(!debug.contains("s3cr3t-leaf"), "{debug}");
        assert!(debug.contains("<unresolved>"), "{debug}");
    }

    /// The host path's own shape: secrets come from a deployments store, and
    /// a member's waits are `after` ∪ derived — the list reconcile turns
    /// into `--after` flags and systemd `After=` directives.
    #[test]
    fn waits_union_explicit_after_with_derived_edges() {
        let db = input("db", Some(PLAIN_MANIFEST));
        let cache = input("cache", Some(PLAIN_MANIFEST));
        let mut server = input("server", Some(PLAIN_MANIFEST));
        server.after = vec!["cache".to_string()];
        server.env = vec![("DB_HOST".to_string(), "{db.host}".to_string())];

        let resolution = resolve_stack(&[db, cache, server], None, false).unwrap();
        assert_eq!(
            resolution.waits["server"],
            vec!["cache".to_string(), "db".to_string()],
            "explicit entries keep their written order; derived edges follow"
        );
    }
}
