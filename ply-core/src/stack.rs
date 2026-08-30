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
}

#[derive(Debug, Clone)]
pub struct Member {
    /// The member identity — the `--name` the instance runs under, and the
    /// handle `after` edges name. Defaults to the image basename of `run`.
    pub name: String,
    pub source: MemberSource,
    /// `-e KEY=VALUE`; VALUE may contain `$VAR`, expanded at launch.
    pub env: Vec<(String, String)>,
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
}

#[derive(Debug)]
pub struct Stack {
    pub name: Option<String>,
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
/// their `<name>.ply` bridge names, secrets are `$VAR` holes. On a laptop
/// almost none of that is true: rootless shares the host network, so the
/// address is loopback and a published port may have to dodge whatever the
/// machine already runs. The overlay is where those local truths live, so
/// the production file never has to carry a dev-shaped lie.
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
            env: parse_env(table.get("e"), &name, path)?,
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
/// A spelled-out stack file has a `[stack]` TABLE; a reference has a `stack`
/// STRING. Same key, different types, so the two can never be confused.
pub fn parse_ref(text: &str) -> Option<String> {
    let doc: toml::Value = text.parse().ok()?;
    match doc.get("stack") {
        Some(toml::Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
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

    let (mut name, mut version, mut description, mut env_file) = (None, None, None, None);
    if let Some(meta) = doc.get("stack") {
        let meta = meta.as_table().ok_or_else(|| {
            Error::Manifest(format!("{}: [stack] must be a table", path.display()))
        })?;
        for key in meta.keys() {
            if !matches!(
                key.as_str(),
                "name" | "version" | "description" | "env_file"
            ) {
                return Err(Error::Manifest(format!(
                    "{}: [stack] has unknown key `{key}` (expected name, version, description, env_file)",
                    path.display()
                )));
            }
        }
        name = meta
            .get("name")
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

    // after edges must name members
    let names: BTreeSet<&str> = members.iter().map(|m| m.name.as_str()).collect();
    for member in &members {
        for dep in &member.after {
            if !names.contains(dep.as_str()) {
                return Err(Error::Manifest(format!(
                    "{}: member `{}` waits for `{dep}`, which is not a member",
                    path.display(),
                    member.name
                )));
            }
            if dep == &member.name {
                return Err(Error::Manifest(format!(
                    "{}: member `{}` waits for itself",
                    path.display(),
                    member.name
                )));
            }
        }
    }

    Ok(Some(Stack {
        name,
        version,
        description,
        env_file,
        members: topo_sort(members, path)?,
    }))
}

const MEMBER_KEYS: &[&str] = &[
    "run", "name", "e", "after", "publish", "volume", "domain", "scale",
];

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

    let env = parse_env(table.get("e"), &name, path)?;
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

    Ok(Member {
        name,
        source,
        env,
        after,
        publish,
        volume,
        domain,
        scale,
    })
}

/// Classify a `run =` value into a source, returning the default member name.
fn classify_run(run: &str, path: &Path, index: usize) -> Result<(MemberSource, Option<String>)> {
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
            "{}: [[app]] #{}: `run = \"{run}\"` is not a package reference, path, or URL",
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

/// Kahn's algorithm, declaration order as the tie-break; cycles are errors.
fn topo_sort(members: Vec<Member>, path: &Path) -> Result<Vec<Member>> {
    let index: BTreeMap<String, usize> = members
        .iter()
        .enumerate()
        .map(|(i, m)| (m.name.clone(), i))
        .collect();
    let mut indegree = vec![0usize; members.len()];
    for member in &members {
        let i = index[&member.name];
        indegree[i] = member.after.len();
    }
    let mut order: Vec<usize> = Vec::with_capacity(members.len());
    let mut ready: Vec<usize> = (0..members.len()).filter(|i| indegree[*i] == 0).collect();
    while let Some(next) = ready.first().copied() {
        ready.remove(0);
        order.push(next);
        for (i, member) in members.iter().enumerate() {
            if member.after.contains(&members[next].name) {
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
                    grew |= wanted.insert(dep.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stack_of(text: &str) -> Stack {
        parse(text, Path::new("ply.toml")).unwrap().unwrap()
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
    fn rejects_cycles() {
        let err = parse(
            "[[app]]\nrun = \"x\"\nname=\"a\"\nafter = \"b\"\n[[app]]\nrun = \"y\"\nname=\"b\"\nafter = \"a\"\n",
            Path::new("p"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("cycle"));
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

    // --- $VAR expansion ------------------------------------------------------

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
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
            after: vec![],
            publish: vec![],
            volume: vec![],
            domain: vec![],
            scale: None,
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
}

#[cfg(test)]
mod stack_ref_tests {
    use super::*;

    /// A deployment file that NAMES a published stack.
    #[test]
    fn a_reference_file_yields_the_reference() {
        assert_eq!(
            parse_ref("stack = \"iluxav/todos\"\n").as_deref(),
            Some("iluxav/todos")
        );
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
