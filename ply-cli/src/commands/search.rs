//! `ply search` — cargo search for containers, over a static catalog file.

use std::path::Path;

use anyhow::{Context, Result};
use ply_core::catalog::{Catalog, Package, OFFICIAL_SOURCE};
use ply_core::manifest::Manifest;
use ply_core::source::Source;
use serde::Serialize;

use crate::cli::SearchArgs;
use crate::commands::build::human_size;

/// Where to search: --source, else ./ply.toml's `[sources] default`, else the
/// official registry. Returns the parsed source and the spec it came from.
pub(crate) fn resolve_source(explicit: Option<&str>, dir: &Path) -> Result<(Source, String)> {
    let spec = match explicit {
        Some(s) => s.to_string(),
        None => {
            let manifest = dir.join("ply.toml");
            if manifest.is_file() {
                let m = Manifest::load(&manifest)?;
                m.sources
                    .get("default")
                    .cloned()
                    .unwrap_or_else(|| OFFICIAL_SOURCE.to_string())
            } else {
                OFFICIAL_SOURCE.to_string()
            }
        }
    };
    let source = Source::parse(&spec, false)?;
    Ok((source, spec))
}

fn toml_key(name: &str) -> String {
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        name.to_string()
    } else {
        format!("\"{name}\"")
    }
}

/// The paste-ready `[dependencies]` line for a package at a range.
pub(crate) fn dep_line(pkg: &Package, range: &str) -> String {
    let key = toml_key(&pkg.name);
    if pkg.namespace.is_empty() || pkg.namespace == "ply" {
        format!("{key} = \"{range}\"")
    } else {
        format!(
            "{key} = {{ source = \"{}\", version = \"{range}\" }}",
            pkg.namespace
        )
    }
}

const DESC_MAX: usize = 60;

fn short_desc(desc: &str) -> String {
    let desc = desc.trim();
    if desc.is_empty() {
        return "(no description)".to_string();
    }
    let chars: Vec<char> = desc.chars().collect();
    if chars.len() <= DESC_MAX {
        desc.to_string()
    } else {
        format!("{}…", chars[..DESC_MAX - 1].iter().collect::<String>())
    }
}

/// One line per package: `name = "range"   # description   arches`.
pub(crate) fn render_rows(pkgs: &[Package]) -> String {
    let lefts: Vec<String> = pkgs
        .iter()
        .map(|p| match p.range_of_latest() {
            Some(r) => dep_line(p, &r),
            None => p.name.clone(),
        })
        .collect();
    let descs: Vec<String> = pkgs.iter().map(|p| short_desc(&p.description)).collect();
    let lw = lefts.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let dw = descs.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let mut out = String::new();
    for (i, p) in pkgs.iter().enumerate() {
        let arches = p.arches_of_latest().join(" ");
        let line = format!("{:<lw$}   # {:<dw$}   {arches}", lefts[i], descs[i]);
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// `--versions`: header line then one row per version, newest first.
pub(crate) fn render_versions(p: &Package) -> String {
    let mut out = format!("{}   {}", p.name, short_desc(&p.description));
    if !p.license.is_empty() {
        out.push_str(&format!("   ({})", p.license));
    }
    out.push('\n');
    let rows = p.versions_desc();
    let vw = rows
        .iter()
        .map(|r| r.version.chars().count())
        .max()
        .unwrap_or(0);
    let aw = rows
        .iter()
        .map(|r| r.arches.join(" ").len())
        .max()
        .unwrap_or(0);
    for r in rows {
        let size = if r.bytes == 0 {
            "—".to_string()
        } else {
            human_size(r.bytes)
        };
        out.push_str(&format!(
            "  {:<vw$}   {:<aw$}   {size}\n",
            r.version,
            r.arches.join(" ")
        ));
    }
    out
}

#[derive(Serialize)]
pub(crate) struct SearchHit<'a> {
    #[serde(flatten)]
    package: &'a Package,
    latest: Option<String>,
    range: Option<String>,
    arches: Vec<&'a str>,
}

impl<'a> SearchHit<'a> {
    pub(crate) fn new(package: &'a Package) -> Self {
        SearchHit {
            package,
            latest: package.latest().map(|v| v.version.clone()),
            range: package.range_of_latest(),
            arches: package.arches_of_latest(),
        }
    }
}

pub fn exec(args: SearchArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("current directory")?;
    let (source, _) = resolve_source(args.source.as_deref(), &cwd)?;
    let catalog = Catalog::load(&source)?;
    let hits = catalog.search(&args.query);
    let total = hits.len();
    let shown: Vec<Package> = hits
        .into_iter()
        .take(if args.limit == 0 {
            usize::MAX
        } else {
            args.limit
        })
        .cloned()
        .collect();

    if args.json {
        let out: Vec<SearchHit<'_>> = shown.iter().map(SearchHit::new).collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if shown.is_empty() {
        eprintln!("no packages match \"{}\"", args.query);
        return Ok(());
    }
    if args.versions {
        for (i, p) in shown.iter().enumerate() {
            if i > 0 {
                println!();
            }
            print!("{}", render_versions(p));
        }
    } else {
        print!("{}", render_rows(&shown));
    }
    if total > shown.len() {
        eprintln!(
            "… and {} more — narrow the query or pass --limit 0",
            total - shown.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ply_core::catalog::ImageVersion;

    fn pkg(name: &str, ns: &str, desc: &str, versions: &[(&str, &str, u64)]) -> Package {
        Package {
            namespace: ns.into(),
            name: name.into(),
            description: desc.into(),
            license: "MIT".into(),
            versions: versions
                .iter()
                .map(|(v, img, b)| ImageVersion {
                    version: (*v).into(),
                    img: (*img).into(),
                    bytes: *b,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn dep_line_forms() {
        let p = pkg("ffmpeg", "ply", "", &[]);
        assert_eq!(dep_line(&p, "6.1"), r#"ffmpeg = "6.1""#);
        let p = pkg("ffmpeg", "corp", "", &[]);
        assert_eq!(
            dep_line(&p, "6.1"),
            r#"ffmpeg = { source = "corp", version = "6.1" }"#
        );
        let p = pkg("py3.12-tools", "ply", "", &[]);
        assert_eq!(dep_line(&p, "1.0"), r#""py3.12-tools" = "1.0""#);
    }

    #[test]
    fn rows_align_and_truncate() {
        let long = "x".repeat(80);
        let pkgs = vec![
            pkg(
                "ffmpeg",
                "ply",
                "Multimedia framework",
                &[
                    ("6.1.1", "ffmpeg-6.1.1-linux-x64.img", 0),
                    ("6.1.1", "ffmpeg-6.1.1-linux-arm64.img", 0),
                ],
            ),
            pkg(
                "ffmpeg-libs",
                "ply",
                &long,
                &[("6.1.1", "ffmpeg-libs-6.1.1-linux-x64.img", 0)],
            ),
            pkg("empty", "ply", "", &[]),
        ];
        let out = render_rows(&pkgs);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with(r#"ffmpeg = "6.1""#), "{}", lines[0]);
        assert!(lines[0].contains("# Multimedia framework"), "{}", lines[0]);
        assert!(
            lines[0].ends_with("x64 arm64"),
            "arches of the latest version: {}",
            lines[0]
        );
        assert!(
            lines[1].starts_with(r#"ffmpeg-libs = "6.1"   # "#),
            "{}",
            lines[1]
        );
        assert!(
            lines[1].contains(&format!("{}…", "x".repeat(59))),
            "60-char cap with ellipsis: {}",
            lines[1]
        );
        assert!(lines[1].ends_with("x64"), "{}", lines[1]);
        assert!(
            lines[2].starts_with("empty "),
            "no versions → bare name: {}",
            lines[2]
        );
        assert!(
            lines[2].ends_with("# (no description)"),
            "trailing arches column trimmed: {}",
            lines[2]
        );
        let col = lines[0].find('#').unwrap();
        assert_eq!(lines[1].find('#').unwrap(), col, "comment column aligned");
        assert_eq!(lines[2].find('#').unwrap(), col, "comment column aligned");
    }

    #[test]
    fn versions_block() {
        let p = pkg(
            "jq",
            "ply",
            "JSON processor",
            &[
                ("1.7.1", "jq-1.7.1-linux-x64.img", 503_808),
                ("1.7.1", "jq-1.7.1-linux-arm64.img", 520_000),
                ("1.6.0", "jq-1.6.0-linux-x64.img", 0),
            ],
        );
        let out = render_versions(&p);
        assert_eq!(
            out,
            "jq   JSON processor   (MIT)\n  1.7.1   x64 arm64   507.8 KiB\n  1.6.0   x64         —\n"
        );
    }

    #[test]
    fn json_hits_carry_latest_and_range() {
        let p = pkg("jq", "ply", "", &[("1.7.1", "jq-1.7.1-linux-x64.img", 1)]);
        let hit = SearchHit::new(&p);
        let v = serde_json::to_value(&hit).unwrap();
        assert_eq!(v["name"], "jq");
        assert_eq!(v["latest"], "1.7.1");
        assert_eq!(v["range"], "1.7");
        assert_eq!(v["arches"], serde_json::json!(["x64"]));
    }

    #[test]
    fn resolve_source_order() {
        let dir = tempfile::tempdir().unwrap();
        let (_, spec) = resolve_source(None, dir.path()).unwrap();
        assert_eq!(spec, OFFICIAL_SOURCE, "no manifest → official");
        std::fs::write(
            dir.path().join("ply.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nbase = \"alpine@3.20\"\n[sources]\ndefault = \"https://corp.example/ply/{package}\"\n",
        )
        .unwrap();
        let (_, spec) = resolve_source(None, dir.path()).unwrap();
        assert_eq!(
            spec, "https://corp.example/ply/{package}",
            "manifest default wins"
        );
        let (_, spec) = resolve_source(Some("file:///tmp/x/{package}"), dir.path()).unwrap();
        assert_eq!(
            spec, "file:///tmp/x/{package}",
            "--source wins over everything"
        );
    }
}
