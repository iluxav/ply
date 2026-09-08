//! `ply init` — write a starter ply.toml, npm-init style.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use ply_core::catalog::{Catalog, OFFICIAL_SOURCE};
use ply_core::source::Source;

use crate::cli::InitArgs;

/// The runtimes the detector can name, with the range `init` falls back to
/// when the registry cannot be reached. The registry's own latest wins
/// whenever it answers; a builtin range only has to be plausible.
const RUNTIMES: &[(&str, &str)] = &[
    ("node", "24"),
    ("python3", "3.13"),
    ("go", "1.24"),
    ("rust", "1.85"),
    ("ruby", "3.3"),
    ("deno", "2.9"),
    ("bun", "1.4"),
];

/// Latest `major.minor` ranges for the packages `init` suggests, and which
/// of them the registry actually carries.
#[derive(Debug, Clone)]
pub(crate) struct Latest {
    pub debian: String,
    ranges: std::collections::BTreeMap<String, String>,
    /// Package names the catalog lists; `None` when the catalog could not
    /// be loaded, in which case nothing is ruled out.
    available: Option<std::collections::BTreeSet<String>>,
}

impl Latest {
    pub(crate) fn builtin() -> Self {
        Latest {
            debian: "13".into(),
            ranges: RUNTIMES
                .iter()
                .map(|(n, r)| (n.to_string(), r.to_string()))
                .collect(),
            available: None,
        }
    }

    pub(crate) fn from_catalog(cat: &Catalog) -> Self {
        let b = Self::builtin();
        let ranges = RUNTIMES
            .iter()
            .map(|(name, _)| {
                let range = cat
                    .get(name)
                    .and_then(|p| p.range_of_latest())
                    .unwrap_or_else(|| b.range(name));
                (name.to_string(), range)
            })
            .collect();
        Latest {
            debian: cat
                .get("debian")
                .and_then(|p| p.range_of_latest())
                .unwrap_or(b.debian),
            ranges,
            available: Some(
                RUNTIMES
                    .iter()
                    .map(|(n, _)| n.to_string())
                    .filter(|n| cat.get(n).is_some())
                    .collect(),
            ),
        }
    }

    /// The range to suggest for a runtime package.
    pub(crate) fn range(&self, package: &str) -> String {
        self.ranges
            .get(package)
            .cloned()
            .unwrap_or_else(|| "*".to_string())
    }

    /// Does the registry carry this runtime? `true` when unknown: a
    /// registry that could not be reached must not turn into a refusal.
    pub(crate) fn has(&self, package: &str) -> bool {
        self.available
            .as_ref()
            .is_none_or(|names| names.contains(package))
    }

    #[cfg(test)]
    fn with_available(mut self, names: &[&str]) -> Self {
        self.available = Some(names.iter().map(|n| n.to_string()).collect());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Defaults {
    pub name: String,
    pub entrypoint: Vec<String>,
    pub runtime: Option<(String, String)>,
    pub port: Option<u16>,
    /// What the detection was based on, for the person to check: "a
    /// package.json", "a go.mod". `None` when nothing was recognised.
    pub evidence: Option<&'static str>,
    /// Environment the runtime needs to behave inside an instance — the
    /// kind of thing a person would only learn from a failure.
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answers {
    pub name: String,
    pub version: String,
    pub entrypoint: Vec<String>,
    pub base: String,
    pub runtime: Option<(String, String)>,
    pub port: Option<u16>,
    pub env: Vec<(String, String)>,
}

/// Lowercase, `[a-z0-9-]` only, runs collapsed, trimmed; `app` if nothing is left.
pub(crate) fn sanitize_name(raw: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in raw.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "app".to_string()
    } else {
        out
    }
}

/// The parsed `package.json`, if there is one that parses.
fn package_json(dir: &Path) -> Option<serde_json::Value> {
    std::fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

/// Does this `package.json` belong to a Bun project? Bun shares the file
/// with Node, so the signal has to be Bun's own: a start script that
/// invokes `bun`, or `packageManager` naming it.
fn package_json_is_bun(pkg: &serde_json::Value) -> bool {
    let start = pkg
        .pointer("/scripts/start")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let manager = pkg
        .get("packageManager")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    start.split_whitespace().next() == Some("bun") || manager.starts_with("bun@")
}

/// The file a Bun start script runs: `bun run src/index.ts` and
/// `bun src/index.ts` both name it; `bun run start` and `bun --hot x.ts`
/// resolve to the first non-flag word after `run`, if any.
fn bun_start_file(pkg: &serde_json::Value) -> Option<String> {
    let start = pkg.pointer("/scripts/start")?.as_str()?;
    let mut words = start.split_whitespace();
    if words.next()? != "bun" {
        return None;
    }
    let mut rest: Vec<&str> = words.filter(|w| !w.starts_with('-')).collect();
    if rest.first() == Some(&"run") {
        rest.remove(0);
    }
    rest.first()
        .filter(|w| w.contains('.'))
        .map(|w| w.to_string())
}

/// The TypeScript file a project with no manifest of its own is run from.
fn ts_entry(dir: &Path) -> Option<&'static str> {
    [
        "index.ts",
        "main.ts",
        "server.ts",
        "src/index.ts",
        "src/main.ts",
    ]
    .into_iter()
    .find(|f| dir.join(f).is_file())
}

/// The file `node` should run: what `npm start` runs when the start script
/// is a plain `node <file>`, else `main`, else the conventional name. A
/// project whose start script is `nodemon` or `next start` is not a file;
/// `main` (or the fallback) stands, and the person edits the line.
fn node_main(dir: &Path) -> String {
    let pkg = std::fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let start = pkg
        .as_ref()
        .and_then(|v| v.pointer("/scripts/start").and_then(|s| s.as_str()))
        .and_then(node_start_file);
    start
        .or_else(|| {
            pkg.as_ref()
                .and_then(|v| v.get("main").and_then(|m| m.as_str()).map(str::to_string))
                .filter(|m| !m.is_empty())
        })
        .unwrap_or_else(|| "server.js".to_string())
}

/// `node index.js` → `index.js`; `node --enable-source-maps dist/app.js` →
/// `dist/app.js`; anything else (`nodemon`, `next start`) → None.
fn node_start_file(script: &str) -> Option<String> {
    let mut words = script.split_whitespace();
    if words.next()? != "node" {
        return None;
    }
    words
        .find(|w| !w.starts_with('-'))
        .filter(|w| w.ends_with(".js") || w.ends_with(".mjs") || w.ends_with(".cjs"))
        .map(str::to_string)
}

/// Does any top-level file carry this extension?
fn has_files_with(dir: &Path, ext: &str) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| e.path().extension().is_some_and(|x| x == ext))
        })
        .unwrap_or(false)
}

/// Does `requirements.txt` or `pyproject.toml` name this package?
fn python_requires(dir: &Path, package: &str) -> bool {
    ["requirements.txt", "pyproject.toml"].iter().any(|f| {
        std::fs::read_to_string(dir.join(f))
            .map(|t| t.to_lowercase().contains(package))
            .unwrap_or(false)
    })
}

/// Filesystem-only project detection.
///
/// One rule per ecosystem, first match wins, in the order a mixed directory
/// is most likely meant: a Node project with a `requirements.txt` for a
/// helper script is a Node project. Every rule names the file it keyed on,
/// so `ply run .` can print "inferred from a go.mod" rather than an
/// unexplained guess.
pub(crate) fn detect(dir: &Path, latest: &Latest) -> Defaults {
    let name = sanitize_name(
        &std::fs::canonicalize(dir)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default(),
    );
    let runtime = |pkg: &str| Some((pkg.to_string(), latest.range(pkg)));
    let has = |f: &str| dir.join(f).is_file();

    // Bun shares package.json with Node, so Bun's own files decide first:
    // a lockfile or a bunfig, then a package.json that itself says `bun`.
    if has("bun.lockb") || has("bun.lock") || has("bunfig.toml") {
        let main = package_json(dir)
            .as_ref()
            .and_then(bun_start_file)
            .or_else(|| ts_entry(dir).map(str::to_string))
            .unwrap_or_else(|| "index.ts".into());
        return Defaults {
            name,
            entrypoint: vec!["bun".into(), "run".into(), main],
            runtime: runtime("bun"),
            port: Some(3000),
            evidence: Some("a bun lockfile"),
            env: Vec::new(),
        };
    }
    if let Some(pkg) = package_json(dir).filter(package_json_is_bun) {
        let main = bun_start_file(&pkg)
            .or_else(|| ts_entry(dir).map(str::to_string))
            .unwrap_or_else(|| "index.ts".into());
        return Defaults {
            name,
            entrypoint: vec!["bun".into(), "run".into(), main],
            runtime: runtime("bun"),
            port: Some(3000),
            evidence: Some("a package.json whose start script is bun"),
            env: Vec::new(),
        };
    }
    if has("package.json") {
        // Next, Nuxt and friends all serve on 3000 by default, as does the
        // plain `http` server every tutorial writes; nothing to detect.
        return Defaults {
            name,
            entrypoint: vec!["node".into(), node_main(dir)],
            runtime: runtime("node"),
            port: Some(3000),
            evidence: Some("a package.json"),
            env: Vec::new(),
        };
    }
    if has("deno.json") || has("deno.jsonc") {
        let main = ["main.ts", "server.ts", "mod.ts", "index.ts", "main.js"]
            .into_iter()
            .find(|f| has(f))
            .unwrap_or("main.ts");
        return Defaults {
            name,
            entrypoint: vec!["deno".into(), "run".into(), "-A".into(), main.into()],
            runtime: runtime("deno"),
            port: Some(8000),
            evidence: Some("a deno.json"),
            env: Vec::new(),
        };
    }
    // TypeScript with no package.json and no deno.json: Bun runs it with
    // no setup at all, so Bun it is — said out loud in the printed
    // manifest, so a Deno project learns to carry its deno.json.
    if let Some(main) = ts_entry(dir) {
        return Defaults {
            name,
            entrypoint: vec!["bun".into(), "run".into(), main.into()],
            runtime: runtime("bun"),
            port: Some(3000),
            evidence: Some("a TypeScript entry file and no package.json"),
            env: Vec::new(),
        };
    }
    if has("go.mod") {
        // `go run .` inside the instance: the toolchain is a dependency
        // like `node`, the module cache lands in the instance's own
        // writable layer, and the image ships the source, not a binary
        // built on a laptop of the wrong architecture. 8080 is what the
        // Go tutorial, gin and chi all listen on.
        let gotmpdir = format!("/opt/{name}");
        return Defaults {
            name,
            entrypoint: vec!["go".into(), "run".into(), ".".into()],
            runtime: runtime("go"),
            port: Some(8080),
            evidence: Some("a go.mod"),
            // `go run` builds into $TMPDIR and execs the result, and an
            // instance's /tmp is noexec. The app's own prefix is writable
            // and executable, so the build lands there; the module and
            // build caches follow HOME as usual.
            env: vec![("GOTMPDIR".into(), gotmpdir)],
        };
    }
    if has("Cargo.toml") {
        return Defaults {
            name,
            entrypoint: vec!["cargo".into(), "run".into(), "--release".into()],
            runtime: runtime("rust"),
            port: Some(8080),
            evidence: Some("a Cargo.toml"),
            env: Vec::new(),
        };
    }
    if has("Gemfile") {
        let (entrypoint, port) = if has("config.ru") {
            (vec!["rackup".into(), "-o".into(), "0.0.0.0".into()], 9292)
        } else {
            let main = ["app.rb", "main.rb", "server.rb"]
                .into_iter()
                .find(|f| has(f))
                .unwrap_or("app.rb");
            (vec!["ruby".into(), main.into()], 4567)
        };
        return Defaults {
            name,
            entrypoint,
            runtime: runtime("ruby"),
            port: Some(port),
            evidence: Some("a Gemfile"),
            env: Vec::new(),
        };
    }
    if has("requirements.txt") || has("pyproject.toml") || has_files_with(dir, "py") {
        // Django keys on manage.py and serves on 8000; Flask's `app.run()`
        // default is 5000; anything else is the stdlib server's 8000.
        let (entrypoint, port) = if has("manage.py") {
            (
                vec![
                    "python3".into(),
                    "manage.py".into(),
                    "runserver".into(),
                    "0.0.0.0:8000".into(),
                ],
                8000,
            )
        } else {
            let script = if !has("app.py") && has("main.py") {
                "main.py"
            } else {
                "app.py"
            };
            let port = if python_requires(dir, "flask") {
                5000
            } else {
                8000
            };
            (vec!["python3".into(), script.into()], port)
        };
        return Defaults {
            name,
            entrypoint,
            runtime: runtime("python3"),
            port: Some(port),
            evidence: Some(if has("manage.py") {
                "a manage.py"
            } else {
                "a Python project"
            }),
            env: Vec::new(),
        };
    }
    Defaults {
        name,
        entrypoint: vec!["/bin/sh".into(), "-c".into(), "echo hello from ply".into()],
        runtime: None,
        port: None,
        evidence: None,
        env: Vec::new(),
    }
}

/// A manifest inferred for a directory that has none, for `ply run DIR`.
pub(crate) struct Inferred {
    /// The manifest, exactly as `ply init -y` would write it.
    pub text: String,
    /// What it was inferred from, for the line that says so.
    pub evidence: &'static str,
}

/// Why a directory cannot be run without a manifest.
pub(crate) enum NotInferable {
    /// Nothing in the directory looked like a project. `dockerfile` says
    /// whether a Dockerfile was there to point at `ply import` instead.
    Unrecognised { dockerfile: bool },
    /// A project the detector knows, whose runtime the registry does not
    /// carry yet: the manifest would only fail at `ply build`.
    RuntimeMissing {
        evidence: &'static str,
        package: String,
    },
}

/// Infer a manifest for `dir` the way `ply init -y` would, without writing
/// anything. The registry is consulted for the latest ranges and for
/// whether the runtime exists at all.
pub(crate) fn infer(dir: &Path) -> std::result::Result<Inferred, NotInferable> {
    let latest = latest_versions();
    let defaults = detect(dir, &latest);
    infer_with(dir, &defaults, &latest)
}

fn infer_with(
    dir: &Path,
    defaults: &Defaults,
    latest: &Latest,
) -> std::result::Result<Inferred, NotInferable> {
    let Some(evidence) = defaults.evidence else {
        return Err(NotInferable::Unrecognised {
            dockerfile: dir.join("Dockerfile").is_file(),
        });
    };
    if let Some((package, _)) = &defaults.runtime {
        if !latest.has(package) {
            return Err(NotInferable::RuntimeMissing {
                evidence,
                package: package.clone(),
            });
        }
    }
    let mut sink = std::io::sink();
    let mut no_input = std::io::empty();
    let answers = prompt(&mut no_input, &mut sink, defaults, latest, true)
        .expect("`yes` never reads input and never fails");
    Ok(Inferred {
        text: render_manifest(&answers),
        evidence,
    })
}

fn ask(
    input: &mut impl BufRead,
    out: &mut impl Write,
    label: &str,
    default: &str,
) -> Result<String> {
    write!(out, "{label} [{default}]: ")?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    let line = line.trim();
    Ok(if line.is_empty() {
        default.to_string()
    } else {
        line.to_string()
    })
}

/// npm-init style questions. `yes` returns the defaults without reading input.
pub(crate) fn prompt(
    input: &mut impl BufRead,
    out: &mut impl Write,
    d: &Defaults,
    latest: &Latest,
    yes: bool,
) -> Result<Answers> {
    let base_default = format!("debian@{}", latest.debian);
    if yes {
        return Ok(Answers {
            name: d.name.clone(),
            version: "0.1.0".into(),
            entrypoint: d.entrypoint.clone(),
            base: base_default,
            runtime: d.runtime.clone(),
            port: d.port,
            env: d.env.clone(),
        });
    }
    writeln!(
        out,
        "This writes a ply.toml. Enter accepts the default; `-` answers none."
    )?;
    let name = sanitize_name(&ask(input, out, "package name", &d.name)?);
    let version = loop {
        let v = ask(input, out, "version", "0.1.0")?;
        if semver::Version::parse(&v).is_ok() {
            break v;
        }
        writeln!(out, "  not a version (want MAJOR.MINOR.PATCH, e.g. 0.1.0)")?;
    };
    let entrypoint: Vec<String> = ask(input, out, "entrypoint", &d.entrypoint.join(" "))?
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let base = ask(input, out, "base", &base_default)?;
    let runtime_default = match &d.runtime {
        Some((n, r)) => format!("{n} = \"{r}\""),
        None => "-".to_string(),
    };
    let runtime = match ask(input, out, "runtime", &runtime_default)?.as_str() {
        "-" => None,
        answer if answer == runtime_default => d.runtime.clone(),
        answer => match (&d.runtime, answer.split_once('=')) {
            (_, Some((n, r))) => {
                Some((n.trim().to_string(), r.trim().trim_matches('"').to_string()))
            }
            (Some((n, _)), None) => Some((n.clone(), answer.trim().trim_matches('"').to_string())),
            (None, None) => Some((answer.trim().to_string(), String::new())),
        },
    };
    let runtime = runtime.filter(|(n, r)| !n.is_empty() && !r.is_empty());
    let port_default = d
        .port
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".to_string());
    let port = match ask(input, out, "port", &port_default)?.as_str() {
        "-" => None,
        p => Some(
            p.parse::<u16>()
                .with_context(|| format!("port must be a number, got {p}"))?,
        ),
    };
    Ok(Answers {
        name,
        version,
        entrypoint,
        base,
        runtime,
        port,
        env: d.env.clone(),
    })
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The quickstart's manifest, commented. Must always pass `Manifest::parse`.
pub(crate) fn render_manifest(a: &Answers) -> String {
    let mut t = String::new();
    t.push_str("[package]\n");
    t.push_str(&format!("name = {}\n", toml_str(&a.name)));
    t.push_str(&format!("version = {}\n", toml_str(&a.version)));
    let args: Vec<String> = a.entrypoint.iter().map(|s| toml_str(s)).collect();
    t.push_str(&format!("entrypoint = [{}]\n", args.join(", ")));
    t.push_str(&format!("base = {}\n", toml_str(&a.base)));
    t.push_str("# include = [\"dist/\"]   # ship only these paths (default: everything in this directory)\n");
    if let Some((name, range)) = &a.runtime {
        t.push_str("\n[dependencies]\n");
        t.push_str(&format!("{name} = {}\n", toml_str(range)));
    }
    if !a.env.is_empty() {
        t.push_str("\n[env]\n");
        for (k, v) in &a.env {
            t.push_str(&format!("{k} = {}\n", toml_str(v)));
        }
    }
    if let Some(port) = a.port {
        t.push_str("\n[ports]\n");
        t.push_str(&format!("http = {port}\n"));
    }
    // No [sources]: the official registry is the resolver's fallback
    // (`resolve::source_spec_for`). A manifest declares [sources] when it
    // actually has somewhere else to fetch from — restating the default in
    // every new project taught the concept to people who did not need it.
    t
}

pub(crate) fn latest_versions() -> Latest {
    match Source::parse(OFFICIAL_SOURCE, false).and_then(|s| Catalog::load(&s)) {
        Ok(cat) => Latest::from_catalog(&cat),
        Err(_) => {
            eprintln!("note: could not reach the registry — using built-in defaults");
            Latest::builtin()
        }
    }
}

pub fn exec(args: InitArgs) -> Result<()> {
    let dir = &args.dir;
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let path = dir.join("ply.toml");
    if path.exists() && !args.force {
        bail!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
    }
    let latest = latest_versions();
    let defaults = detect(dir, &latest);
    let yes = args.yes || !std::io::stdin().is_terminal();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    let answers = prompt(&mut input, &mut out, &defaults, &latest, yes)?;
    let text = render_manifest(&answers);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    println!("\n{text}");
    println!("wrote {}", path.display());
    println!(
        "next: ply build {}          # resolve, lock, build the image",
        dir.display()
    );
    println!("      ply add <package>    # add a dependency from the registry");
    println!("      commit ply.lock; ignore *.img");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ply_core::manifest::Manifest;

    fn latest() -> Latest {
        Latest::builtin()
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_name("My App"), "my-app");
        assert_eq!(sanitize_name("  --Weird__Name!!  "), "weird-name");
        assert_eq!(sanitize_name("ok-name"), "ok-name");
        assert_eq!(sanitize_name("!!!"), "app");
    }

    #[test]
    fn detects_node() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"x","main":"dist/index.js"}"#,
        )
        .unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(d.runtime, Some(("node".into(), "24".into())));
        assert_eq!(d.entrypoint, vec!["node", "dist/index.js"]);
        assert_eq!(d.port, Some(3000));
    }

    /// What `npm start` runs is the truest answer: a project with
    /// `"start": "node index.js"` and no `main` got `server.js` on a fresh
    /// droplet, and the first `ply run` failed to find it.
    #[test]
    fn a_plain_node_start_script_names_the_entrypoint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"x","scripts":{"start":"node index.js"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect(dir.path(), &latest()).entrypoint,
            vec!["node", "index.js"]
        );
        assert_eq!(
            node_start_file("node --enable-source-maps dist/app.js"),
            Some("dist/app.js".into())
        );
        assert_eq!(node_start_file("nodemon index.js"), None);
        assert_eq!(node_start_file("next start"), None);
        // start beats main; a non-file start leaves main in charge
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"main":"lib.js","scripts":{"start":"node bin/www.js"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect(dir.path(), &latest()).entrypoint,
            vec!["node", "bin/www.js"]
        );
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"main":"lib.js","scripts":{"start":"next start"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect(dir.path(), &latest()).entrypoint,
            vec!["node", "lib.js"]
        );
    }

    #[test]
    fn node_without_main_falls_back_to_server_js() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(
            detect(dir.path(), &latest()).entrypoint,
            vec!["node", "server.js"]
        );
    }

    #[test]
    fn detects_python() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("requirements.txt"), "requests\n").unwrap();
        std::fs::write(dir.path().join("main.py"), "").unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(d.runtime, Some(("python3".into(), "3.13".into())));
        assert_eq!(
            d.entrypoint,
            vec!["python3", "main.py"],
            "main.py when app.py is absent"
        );
        assert_eq!(d.port, Some(8000));
        std::fs::write(dir.path().join("app.py"), "").unwrap();
        assert_eq!(
            detect(dir.path(), &latest()).entrypoint,
            vec!["python3", "app.py"]
        );
    }

    #[test]
    fn detects_go_and_runs_it_in_the_instance() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("go.mod"),
            "module example.com/hello\n\ngo 1.24\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("main.go"), "package main\nfunc main() {}\n").unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(d.runtime, Some(("go".into(), "1.24".into())));
        assert_eq!(d.entrypoint, vec!["go", "run", "."]);
        assert_eq!(d.port, Some(8080));
        assert_eq!(d.evidence, Some("a go.mod"));
        assert_eq!(
            d.env[0].0, "GOTMPDIR",
            "go run execs its build: not from a noexec /tmp"
        );
    }

    #[test]
    fn django_and_flask_get_their_own_ports_and_entrypoints() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manage.py"), "#!/usr/bin/env python3\n").unwrap();
        std::fs::write(dir.path().join("requirements.txt"), "Django==5.1\n").unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(
            d.entrypoint,
            vec!["python3", "manage.py", "runserver", "0.0.0.0:8000"]
        );
        assert_eq!(d.port, Some(8000));
        assert_eq!(d.evidence, Some("a manage.py"));

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.py"), "from flask import Flask\n").unwrap();
        std::fs::write(dir.path().join("requirements.txt"), "flask\n").unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(d.entrypoint, vec!["python3", "app.py"]);
        assert_eq!(d.port, Some(5000), "Flask's app.run() default");
    }

    #[test]
    fn rust_ruby_deno_and_bun_are_recognised() {
        let cases: &[(&str, &str, &str, &[&str])] = &[
            (
                "Cargo.toml",
                "[package]\nname = \"svc\"\n",
                "rust",
                &["cargo", "run", "--release"],
            ),
            (
                "Gemfile",
                "source 'https://rubygems.org'\n",
                "ruby",
                &["ruby", "app.rb"],
            ),
            ("deno.json", "{}", "deno", &["deno", "run", "-A", "main.ts"]),
            ("bun.lock", "", "bun", &["bun", "run", "index.ts"]),
        ];
        for (file, body, runtime, entrypoint) in cases {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(file), body).unwrap();
            let d = detect(dir.path(), &latest());
            assert_eq!(
                d.runtime.as_ref().map(|(n, _)| n.as_str()),
                Some(*runtime),
                "{file}"
            );
            assert_eq!(d.entrypoint, *entrypoint, "{file}");
            assert!(d.evidence.is_some(), "{file}");
        }
    }

    #[test]
    fn bun_is_told_apart_from_node_by_its_own_signals() {
        // A package.json whose start script is bun: Bun, entrypoint from it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"start":"bun run src/index.ts"}}"#,
        )
        .unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(d.runtime.as_ref().map(|(n, _)| n.as_str()), Some("bun"));
        assert_eq!(d.entrypoint, vec!["bun", "run", "src/index.ts"]);
        // `packageManager` says so too, with the entry file found on disk.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"packageManager":"bun@1.4.2"}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("server.ts"), "").unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(d.entrypoint, vec!["bun", "run", "server.ts"]);
        // A lockfile beats everything, and a plain package.json is still Node.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"main":"app.js"}"#).unwrap();
        std::fs::write(dir.path().join("index.ts"), "").unwrap();
        assert_eq!(
            detect(dir.path(), &latest()).entrypoint,
            vec!["node", "app.js"]
        );
        std::fs::write(dir.path().join("bun.lock"), "").unwrap();
        assert_eq!(
            detect(dir.path(), &latest()).entrypoint,
            vec!["bun", "run", "index.ts"]
        );
    }

    #[test]
    fn a_lone_typescript_file_runs_on_bun_unless_deno_says_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.ts"), "").unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(d.entrypoint, vec!["bun", "run", "index.ts"]);
        assert_eq!(
            d.evidence,
            Some("a TypeScript entry file and no package.json")
        );
        std::fs::write(dir.path().join("deno.json"), "{}").unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(d.runtime.as_ref().map(|(n, _)| n.as_str()), Some("deno"));
        assert_eq!(d.entrypoint, vec!["deno", "run", "-A", "index.ts"]);
    }

    #[test]
    fn a_node_project_wins_over_a_stray_requirements_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{\"main\":\"index.js\"}").unwrap();
        std::fs::write(dir.path().join("requirements.txt"), "requests\n").unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(d.runtime.as_ref().map(|(n, _)| n.as_str()), Some("node"));
    }

    #[test]
    fn inference_refuses_what_it_cannot_run_and_says_why() {
        // Nothing recognisable, with a Dockerfile to point at.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        let latest = latest().with_available(&["node", "python3"]);
        match infer_with(dir.path(), &detect(dir.path(), &latest), &latest) {
            Err(NotInferable::Unrecognised { dockerfile: true }) => {}
            _ => panic!("a Dockerfile alone is not a ply project"),
        }
        // A project whose runtime the registry does not carry: refused
        // here, with the package named, rather than at `ply build`.
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        match infer_with(dir.path(), &detect(dir.path(), &latest), &latest) {
            Err(NotInferable::RuntimeMissing { package, evidence }) => {
                assert_eq!(package, "rust");
                assert_eq!(evidence, "a Cargo.toml");
            }
            _ => panic!("rust is not in the registry"),
        }
        // A registry that could not be reached rules nothing out.
        let unknown = Latest::builtin();
        assert!(infer_with(dir.path(), &detect(dir.path(), &unknown), &unknown).is_ok());
    }

    #[test]
    fn an_inferred_manifest_is_what_init_dash_y_writes_and_parses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{\"main\":\"server.js\"}").unwrap();
        let latest = latest().with_available(&["node"]);
        let inferred = infer_with(dir.path(), &detect(dir.path(), &latest), &latest)
            .ok()
            .expect("a Node directory is inferable");
        assert_eq!(inferred.evidence, "a package.json");
        let m = Manifest::parse(&inferred.text).expect("parses");
        assert_eq!(
            m.package.entrypoint.as_deref(),
            Some(&["node".to_string(), "server.js".to_string()][..])
        );
        assert!(inferred.text.contains("node = \"24\""));
        assert!(inferred.text.contains("http = 3000"));
    }

    #[test]
    fn detects_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(d.runtime, None);
        assert_eq!(d.entrypoint, vec!["/bin/sh", "-c", "echo hello from ply"]);
        assert_eq!(d.port, None);
        assert_eq!(
            d.name,
            sanitize_name(&dir.path().file_name().unwrap().to_string_lossy())
        );
    }

    fn defaults() -> Defaults {
        Defaults {
            name: "myapp".into(),
            entrypoint: vec!["python3".into(), "app.py".into()],
            runtime: Some(("python3".into(), "3.13".into())),
            port: Some(8000),
            evidence: Some("a Python project"),
            env: Vec::new(),
        }
    }

    #[test]
    fn yes_takes_every_default_without_reading_stdin() {
        let mut input = std::io::Cursor::new(b"SHOULD NOT BE READ\n".to_vec());
        let mut out = Vec::new();
        let a = prompt(&mut input, &mut out, &defaults(), &latest(), true).unwrap();
        assert_eq!(a.name, "myapp");
        assert_eq!(a.version, "0.1.0");
        assert_eq!(a.base, "debian@13");
        assert_eq!(a.runtime, Some(("python3".into(), "3.13".into())));
        assert_eq!(a.port, Some(8000));
        assert_eq!(input.position(), 0);
    }

    #[test]
    fn enter_accepts_defaults_and_answers_override() {
        let mut input = std::io::Cursor::new(b"\n1.2.3\nnode server.js\n\n3.11\n\n".to_vec());
        let mut out = Vec::new();
        let a = prompt(&mut input, &mut out, &defaults(), &latest(), false).unwrap();
        assert_eq!(a.name, "myapp");
        assert_eq!(a.version, "1.2.3");
        assert_eq!(a.entrypoint, vec!["node", "server.js"]);
        assert_eq!(a.base, "debian@13");
        assert_eq!(a.runtime, Some(("python3".into(), "3.11".into())));
        assert_eq!(a.port, Some(8000));
        let shown = String::from_utf8(out).unwrap();
        assert!(shown.contains("package name [myapp]:"), "{shown}");
        assert!(shown.contains("runtime [python3 = \"3.13\"]"), "{shown}");
    }

    #[test]
    fn bad_version_is_asked_again_and_empty_runtime_means_none() {
        let mut input = std::io::Cursor::new(b"\nnot-a-version\n0.2.0\n\n\n-\n\n".to_vec());
        let mut out = Vec::new();
        let a = prompt(&mut input, &mut out, &defaults(), &latest(), false).unwrap();
        assert_eq!(a.version, "0.2.0");
        assert_eq!(a.runtime, None, "'-' answers none");
        assert!(String::from_utf8(out).unwrap().contains("not a version"));
    }

    #[test]
    fn rendered_manifest_is_valid_and_round_trips() {
        let a = Answers {
            name: "myapp".into(),
            version: "0.1.0".into(),
            entrypoint: vec!["python3".into(), "app.py".into()],
            base: "debian@13".into(),
            runtime: Some(("python3".into(), "3.13".into())),
            port: Some(8000),
            env: Vec::new(),
        };
        let text = render_manifest(&a);
        let m = Manifest::parse(&text).expect("ply build must accept what init wrote");
        assert_eq!(m.package.name, "myapp");
        assert_eq!(
            m.package.entrypoint.as_deref(),
            Some(&["python3".to_string(), "app.py".to_string()][..])
        );
        assert_eq!(m.ports["http"], 8000);
        assert!(
            m.sources.is_empty(),
            "init must not emit a [sources] stanza"
        );
        assert!(text.contains("# include = [\"dist/\"]"));
        assert!(text.contains("[dependencies]\npython3 = \"3.13\""));
    }

    #[test]
    fn empty_sections_are_omitted() {
        let a = Answers {
            name: "bare".into(),
            version: "0.1.0".into(),
            entrypoint: vec!["/bin/sh".into(), "-c".into(), "echo hi".into()],
            base: "debian@13".into(),
            runtime: None,
            port: None,
            env: Vec::new(),
        };
        let text = render_manifest(&a);
        assert!(!text.contains("[dependencies]"));
        assert!(!text.contains("[ports]"));
        Manifest::parse(&text).unwrap();
    }
}
