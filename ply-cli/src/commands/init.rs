//! `ply init` — write a starter ply.toml, npm-init style.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use ply_core::catalog::{Catalog, OFFICIAL_SOURCE};
use ply_core::source::Source;

use crate::cli::InitArgs;

/// Latest `major.minor` ranges for the packages `init` suggests.
#[derive(Debug, Clone)]
pub(crate) struct Latest {
    pub debian: String,
    pub python3: String,
    pub node: String,
}

impl Latest {
    pub(crate) fn builtin() -> Self {
        Latest {
            debian: "13".into(),
            python3: "3.12".into(),
            node: "24".into(),
        }
    }

    pub(crate) fn from_catalog(cat: &Catalog) -> Self {
        let b = Self::builtin();
        let pick = |name: &str, fallback: String| {
            cat.get(name)
                .and_then(|p| p.range_of_latest())
                .unwrap_or(fallback)
        };
        Latest {
            debian: pick("debian", b.debian),
            python3: pick("python3", b.python3),
            node: pick("node", b.node),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Defaults {
    pub name: String,
    pub entrypoint: Vec<String>,
    pub runtime: Option<(String, String)>,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answers {
    pub name: String,
    pub version: String,
    pub entrypoint: Vec<String>,
    pub base: String,
    pub runtime: Option<(String, String)>,
    pub port: Option<u16>,
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

fn node_main(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("main").and_then(|m| m.as_str()).map(str::to_string))
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "server.js".to_string())
}

fn has_py_files(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| e.path().extension().is_some_and(|x| x == "py"))
        })
        .unwrap_or(false)
}

/// Filesystem-only project detection.
pub(crate) fn detect(dir: &Path, latest: &Latest) -> Defaults {
    let name = sanitize_name(
        &std::fs::canonicalize(dir)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default(),
    );
    if dir.join("package.json").is_file() {
        return Defaults {
            name,
            entrypoint: vec!["node".into(), node_main(dir)],
            runtime: Some(("node".into(), latest.node.clone())),
            port: Some(3000),
        };
    }
    if dir.join("requirements.txt").is_file()
        || dir.join("pyproject.toml").is_file()
        || has_py_files(dir)
    {
        let script = if !dir.join("app.py").is_file() && dir.join("main.py").is_file() {
            "main.py"
        } else {
            "app.py"
        };
        return Defaults {
            name,
            entrypoint: vec!["python3".into(), script.into()],
            runtime: Some(("python3".into(), latest.python3.clone())),
            port: Some(8000),
        };
    }
    Defaults {
        name,
        entrypoint: vec!["/bin/sh".into(), "-c".into(), "echo hello from ply".into()],
        runtime: None,
        port: None,
    }
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
    if let Some(port) = a.port {
        t.push_str("\n[ports]\n");
        t.push_str(&format!("http = {port}\n"));
    }
    t.push_str("\n[sources]\n");
    t.push_str(&format!("default = {}\n", toml_str(OFFICIAL_SOURCE)));
    t
}

fn latest_versions() -> Latest {
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
        std::fs::write(dir.path().join("requirements.txt"), "flask\n").unwrap();
        std::fs::write(dir.path().join("main.py"), "").unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(d.runtime, Some(("python3".into(), "3.12".into())));
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
            runtime: Some(("python3".into(), "3.12".into())),
            port: Some(8000),
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
        assert_eq!(a.runtime, Some(("python3".into(), "3.12".into())));
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
        assert!(shown.contains("runtime [python3 = \"3.12\"]"), "{shown}");
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
            runtime: Some(("python3".into(), "3.12".into())),
            port: Some(8000),
        };
        let text = render_manifest(&a);
        let m = Manifest::parse(&text).expect("ply build must accept what init wrote");
        assert_eq!(m.package.name, "myapp");
        assert_eq!(
            m.package.entrypoint.as_deref(),
            Some(&["python3".to_string(), "app.py".to_string()][..])
        );
        assert_eq!(m.ports["http"], 8000);
        assert_eq!(m.sources["default"], OFFICIAL_SOURCE);
        assert!(text.contains("# include = [\"dist/\"]"));
        assert!(text.contains("[dependencies]\npython3 = \"3.12\""));
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
        };
        let text = render_manifest(&a);
        assert!(!text.contains("[dependencies]"));
        assert!(!text.contains("[ports]"));
        Manifest::parse(&text).unwrap();
    }
}
