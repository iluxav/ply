//! `[stack]` — one file, several instances: the compose equivalent.
//!
//! A stack ply.toml is pure wiring; it defines no app of its own. Each
//! member is either a prebuilt runnable app from the registry (`run =
//! "postgres@17"`) or a local directory whose own ply.toml defines the app
//! (`path = "./server"`). `after` edges order startup and ride the existing
//! `--after` readiness gate; per-member `env` rides `-e`.
//!
//! Registry members are version-locked in the stack dir's ply.lock —
//! upgrades are deliberate (`ply up --refresh`), same principle as MVS.

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
    /// `path = "./server"` — a local app dir, built at `up` time.
    Path(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Member {
    /// The stack key (`db`, `server`); also the handle `after` edges name.
    pub name: String,
    pub source: MemberSource,
    pub env: Vec<(String, String)>,
    /// Stack member keys this one waits for.
    pub after: Vec<String>,
}

#[derive(Debug)]
pub struct Stack {
    /// Members in dependency order (dependencies before dependants).
    pub members: Vec<Member>,
}

/// Read `<dir>/ply.toml` as a stack. `Ok(None)` when the file has no
/// `[stack]` table (it's an app manifest or absent).
pub fn load(dir: &Path) -> Result<Option<Stack>> {
    let path = dir.join("ply.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    parse(&text, &path)
}

fn parse(text: &str, path: &Path) -> Result<Option<Stack>> {
    let doc: toml::Value = text
        .parse()
        .map_err(|e| Error::Manifest(format!("{}: {e}", path.display())))?;
    let Some(stack) = doc.get("stack") else {
        return Ok(None);
    };
    if doc.get("package").is_some() {
        return Err(Error::Manifest(format!(
            "{}: has both [package] and [stack] — a stack file is pure wiring; move the app into its own directory and reference it with `path`",
            path.display()
        )));
    }
    let table = stack.as_table().ok_or_else(|| {
        Error::Manifest(format!(
            "{}: [stack] must be a table of members",
            path.display()
        ))
    })?;
    if table.is_empty() {
        return Err(Error::Manifest(format!(
            "{}: [stack] has no members",
            path.display()
        )));
    }

    let mut members = Vec::new();
    for (name, entry) in table {
        members.push(parse_member(name, entry, path)?);
    }

    // after edges must name members
    let names: BTreeSet<&str> = members.iter().map(|m| m.name.as_str()).collect();
    for member in &members {
        for dep in &member.after {
            if !names.contains(dep.as_str()) {
                return Err(Error::Manifest(format!(
                    "{}: member `{}` waits for `{dep}`, which is not a [stack] member",
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
        members: topo_sort(members, path)?,
    }))
}

fn parse_member(name: &str, entry: &toml::Value, path: &Path) -> Result<Member> {
    let table = entry.as_table().ok_or_else(|| {
        Error::Manifest(format!(
            "{}: member `{name}` must be a table, e.g. {{ run = \"postgres@17\" }}",
            path.display()
        ))
    })?;
    for key in table.keys() {
        if !matches!(key.as_str(), "run" | "path" | "env" | "after") {
            return Err(Error::Manifest(format!(
                "{}: member `{name}`: unknown key `{key}` (expected run, path, env, after)",
                path.display()
            )));
        }
    }
    let run = table.get("run").map(|v| v.as_str());
    let dir = table.get("path").map(|v| v.as_str());
    let source = match (run, dir) {
        (Some(Some(reference)), None) => {
            let Some((pkg, want)) = crate::catalog::parse_run_ref(reference) else {
                return Err(Error::Manifest(format!(
                    "{}: member `{name}`: `run = \"{reference}\"` is not a package reference (expected name or name@version)",
                    path.display()
                )));
            };
            MemberSource::Run {
                name: pkg,
                version: want,
            }
        }
        (None, Some(Some(p))) => MemberSource::Path(PathBuf::from(p)),
        (None, None) => {
            return Err(Error::Manifest(format!(
                "{}: member `{name}` needs `run = \"name@ver\"` or `path = \"./dir\"`",
                path.display()
            )))
        }
        (Some(_), Some(_)) => {
            return Err(Error::Manifest(format!(
                "{}: member `{name}` has both `run` and `path` — pick one",
                path.display()
            )))
        }
        _ => {
            return Err(Error::Manifest(format!(
                "{}: member `{name}`: `run`/`path` must be strings",
                path.display()
            )))
        }
    };

    let mut env = Vec::new();
    if let Some(value) = table.get("env") {
        let env_table = value.as_table().ok_or_else(|| {
            Error::Manifest(format!(
                "{}: member `{name}`: env must be a table of KEY = \"value\"",
                path.display()
            ))
        })?;
        for (k, v) in env_table {
            let text = match v {
                toml::Value::String(s) => s.clone(),
                toml::Value::Integer(i) => i.to_string(),
                toml::Value::Float(f) => f.to_string(),
                toml::Value::Boolean(b) => b.to_string(),
                _ => {
                    return Err(Error::Manifest(format!(
                        "{}: member `{name}`: env {k} must be a string or number",
                        path.display()
                    )))
                }
            };
            env.push((k.clone(), text));
        }
    }

    let after = match table.get("after") {
        None => Vec::new(),
        Some(toml::Value::String(one)) => vec![one.clone()],
        Some(toml::Value::Array(list)) => {
            let mut out = Vec::new();
            for item in list {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => {
                        return Err(Error::Manifest(format!(
                            "{}: member `{name}`: after must name members (strings)",
                            path.display()
                        )))
                    }
                }
            }
            out
        }
        Some(_) => {
            return Err(Error::Manifest(format!(
                "{}: member `{name}`: after must be a member name or an array of them",
                path.display()
            )))
        }
    };

    Ok(Member {
        name: name.to_string(),
        source,
        env,
        after,
    })
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
            "{}: [stack] has an `after` cycle involving {}",
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
                "`{name}` is not a [stack] member (members: {})",
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
db     = { run = "postgres@17", env = { POSTGRES_PASSWORD = "dev", PGPORT = 5442 } }
server = { path = "./server", after = "db" }
web    = { path = "./web", after = "server" }
"#;

    #[test]
    fn parses_and_orders_the_lab_stack() {
        let stack = stack_of(LAB);
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
        // numbers coerce to strings for -e
        assert!(db.env.contains(&("PGPORT".into(), "5442".into())));
        assert_eq!(stack.members[1].after, vec!["db"]);
    }

    #[test]
    fn no_stack_table_is_none() {
        assert!(parse("[package]\nname = \"x\"\n", Path::new("p"))
            .unwrap()
            .is_none());
        assert!(parse("", Path::new("p")).unwrap().is_none());
    }

    #[test]
    fn rejects_package_plus_stack() {
        let err = parse(
            "[package]\nname = \"x\"\n[stack]\ndb = { run = \"redis\" }\n",
            Path::new("p"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("pure wiring"));
    }

    #[test]
    fn rejects_bad_members() {
        for (toml, msg) in [
            ("[stack]\ndb = { }\n", "needs `run"),
            (
                "[stack]\ndb = { run = \"redis\", path = \"./x\" }\n",
                "pick one",
            ),
            (
                "[stack]\ndb = { run = \"redis\", scale = 2 }\n",
                "unknown key",
            ),
            (
                "[stack]\ndb = { run = \"Not A Ref\" }\n",
                "not a package reference",
            ),
            (
                "[stack]\na = { run = \"redis\", after = \"ghost\" }\n",
                "not a [stack] member",
            ),
            (
                "[stack]\na = { run = \"redis\", after = \"a\" }\n",
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
            "[stack]\na = { run = \"x\", after = \"b\" }\nb = { run = \"y\", after = \"a\" }\n",
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

    #[test]
    fn lock_roundtrip_and_pin_invalidation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = StackLock::default();
        lock.record("db", "postgres@17", "17.10.0", "x64", "sha256:abc");
        lock.record("db", "postgres@17", "17.10.0", "arm64", "sha256:def");
        lock.save(tmp.path()).unwrap();
        let loaded = StackLock::load(tmp.path());
        assert_eq!(loaded, lock);
        // same reference → pinned, with per-arch digests
        let pin = loaded.pinned("db", "postgres@17").unwrap();
        assert_eq!(pin.version, "17.10.0");
        assert_eq!(pin.digests.get("x64"), Some(&"sha256:abc".to_string()));
        assert_eq!(pin.digests.get("arm64"), Some(&"sha256:def".to_string()));
        // changed reference → re-resolve
        assert!(loaded.pinned("db", "postgres@18").is_none());
        assert!(loaded.pinned("other", "postgres@17").is_none());
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
