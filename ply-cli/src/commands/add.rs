//! `ply add` — write a dependency into ./ply.toml, format-preserving.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use ply_core::catalog::{Catalog, OFFICIAL_SOURCE};
use ply_core::source::Source;
use toml_edit::{value, DocumentMut, InlineTable, Item, Table};

use crate::cli::AddArgs;

#[derive(Debug)]
pub(crate) enum Outcome {
    Added,
    Updated { from: String },
    Unchanged,
}

#[derive(Debug)]
pub(crate) struct Edit {
    pub outcome: Outcome,
    /// The line as written, e.g. `ffmpeg = "6.1"`.
    pub line: String,
}

/// `name` or `name@range`.
pub(crate) fn parse_spec(spec: &str) -> Result<(String, Option<String>)> {
    match spec.split_once('@') {
        None if !spec.is_empty() => Ok((spec.to_string(), None)),
        Some((name, range)) if !name.is_empty() && !range.is_empty() => {
            Ok((name.to_string(), Some(range.to_string())))
        }
        _ => bail!("expected <name> or <name>@<range>, got \"{spec}\""),
    }
}

/// Re-number top-level table positions so `new_key` renders right after
/// `after_key`. toml_edit appends new tables at the end by default.
pub(crate) fn place_after(doc: &mut DocumentMut, new_key: &str, after_key: &str) {
    let mut order: Vec<(String, usize)> = doc
        .as_table()
        .iter()
        .filter(|(k, _)| *k != new_key)
        .filter_map(|(k, item)| {
            item.as_table()
                .map(|t| (k.to_string(), t.position().unwrap_or(usize::MAX)))
        })
        .collect();
    order.sort_by_key(|(_, pos)| *pos);
    let mut keys: Vec<String> = order.into_iter().map(|(k, _)| k).collect();
    let at = keys
        .iter()
        .position(|k| k == after_key)
        .map(|i| i + 1)
        .unwrap_or(keys.len());
    keys.insert(at, new_key.to_string());
    for (i, k) in keys.iter().enumerate() {
        if let Some(t) = doc.get_mut(k).and_then(Item::as_table_mut) {
            t.set_position(i);
        }
    }
}

fn render_value(item: &Item) -> String {
    item.to_string().trim().to_string()
}

/// Pure edit: add/update `name = range` under `[dependencies]`.
pub(crate) fn apply_add(
    doc: &mut DocumentMut,
    name: &str,
    range: &str,
    source: Option<&str>,
) -> Result<Edit> {
    if let Some(src) = source {
        let have: Vec<String> = doc
            .get("sources")
            .and_then(Item::as_table)
            .map(|t| t.iter().map(|(k, _)| k.to_string()).collect())
            .unwrap_or_default();
        if !have.contains(&src.to_string()) {
            bail!(
                "no source \"{src}\" in [sources] (have: {})",
                have.join(", ")
            );
        }
    }
    // Nothing to insert when no `source` was named: the official registry is
    // the resolver's fallback, so a plain `ply add redis` leaves the manifest
    // with just the dependency line.

    let new_item: Item = match source {
        None => value(range),
        Some(src) => {
            let mut t = InlineTable::new();
            t.insert("source", src.into());
            t.insert("version", range.into());
            value(t)
        }
    };

    let had_deps = doc.get("dependencies").and_then(Item::as_table).is_some();
    if !had_deps {
        doc["dependencies"] = Item::Table(Table::new());
        place_after(doc, "dependencies", "package");
    }
    let deps = doc["dependencies"].as_table_mut().expect("just ensured");
    let outcome = match deps.get(name) {
        Some(old) if render_value(old) == render_value(&new_item) => Outcome::Unchanged,
        Some(old) => Outcome::Updated {
            from: render_value(old).trim_matches('"').to_string(),
        },
        None => Outcome::Added,
    };
    if !matches!(outcome, Outcome::Unchanged) {
        deps.insert(name, new_item.clone());
    }
    let key = toml_edit::Key::new(name).to_string();
    let line = format!("{} = {}", key.trim(), render_value(&new_item));
    Ok(Edit { outcome, line })
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

pub fn exec(args: AddArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("current directory")?;
    let path = cwd.join("ply.toml");
    if !path.is_file() {
        bail!("no ply.toml in {}", cwd.display());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("{}: not valid TOML", path.display()))?;
    let (name, range) = parse_spec(&args.spec)?;

    let range = match range {
        Some(r) => r,
        None => {
            let key = args.source.as_deref().unwrap_or("default");
            let spec = doc
                .get("sources")
                .and_then(Item::as_table)
                .and_then(|t| t.get(key))
                .and_then(Item::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| OFFICIAL_SOURCE.to_string());
            if args.source.is_some()
                && !doc
                    .get("sources")
                    .and_then(Item::as_table)
                    .map(|t| t.contains_key(key))
                    .unwrap_or(false)
            {
                let have: Vec<String> = doc
                    .get("sources")
                    .and_then(Item::as_table)
                    .map(|t| t.iter().map(|(k, _)| k.to_string()).collect())
                    .unwrap_or_default();
                bail!(
                    "no source \"{key}\" in [sources] (have: {})",
                    have.join(", ")
                );
            }
            let source = Source::parse(&spec, false)?;
            let catalog = Catalog::load(&source)?;
            let prefix = source.catalog_location()?.prefix();
            catalog
                .get(&name)
                .ok_or_else(|| {
                    anyhow!("no package \"{name}\" in {prefix} — try: ply search {name}")
                })?
                .range_of_latest()
                .ok_or_else(|| {
                    anyhow!("package \"{name}\" in {prefix} has no published versions")
                })?
        }
    };

    let edit = apply_add(&mut doc, &name, &range, args.source.as_deref())?;
    match edit.outcome {
        Outcome::Unchanged => {
            println!("{} already in ply.toml", edit.line);
            return Ok(());
        }
        Outcome::Updated { ref from } => println!("{name} \"{from}\" → \"{range}\" in ply.toml"),
        Outcome::Added => println!("added {} to ply.toml", edit.line),
    }
    write_atomic(&path, &doc.to_string())?;
    println!("run ply build to resolve and lock");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "# my app\n[package]\nname = \"a\"\nversion = \"0.1.0\"\nbase = \"alpine@3.20\"\n\n[ports]\nhttp = 8000   # keep\n\n[sources]\ndefault = \"https://registry.plybox.sh/ply/{package}\"\n";

    fn doc(s: &str) -> DocumentMut {
        s.parse().unwrap()
    }

    #[test]
    fn creates_dependencies_right_after_package_and_keeps_comments() {
        let mut d = doc(BASE);
        let e = apply_add(&mut d, "ffmpeg", "6.1", None).unwrap();
        assert!(matches!(e.outcome, Outcome::Added));
        assert_eq!(e.line, r#"ffmpeg = "6.1""#);
        let out = d.to_string();
        let pkg = out.find("[package]").unwrap();
        let deps = out.find("[dependencies]").unwrap();
        let ports = out.find("[ports]").unwrap();
        assert!(
            pkg < deps && deps < ports,
            "placed after [package], before [ports]:\n{out}"
        );
        assert!(out.contains("# my app"), "leading comment survives");
        assert!(
            out.contains("http = 8000   # keep"),
            "inline comment survives"
        );
        assert!(out.contains("ffmpeg = \"6.1\""));
    }

    #[test]
    fn updates_existing_entry_and_reports_old_value() {
        let mut d = doc(&format!("{BASE}[dependencies]\nffmpeg = \"6.0\"\n"));
        let e = apply_add(&mut d, "ffmpeg", "6.1", None).unwrap();
        assert!(matches!(e.outcome, Outcome::Updated { ref from } if from == "6.0"));
        assert!(d.to_string().contains("ffmpeg = \"6.1\""));
        assert!(!d.to_string().contains("6.0"));
    }

    #[test]
    fn unchanged_when_already_present() {
        let mut d = doc(&format!("{BASE}[dependencies]\nffmpeg = \"6.1\"\n"));
        let before = d.to_string();
        let e = apply_add(&mut d, "ffmpeg", "6.1", None).unwrap();
        assert!(matches!(e.outcome, Outcome::Unchanged));
        assert_eq!(d.to_string(), before);
    }

    #[test]
    fn table_form_for_named_source() {
        let mut d = doc(&BASE.replace(
            "[sources]\n",
            "[sources]\ncorp = \"https://corp/{package}\"\n",
        ));
        let e = apply_add(&mut d, "ffmpeg", "6.1", Some("corp")).unwrap();
        assert_eq!(e.line, r#"ffmpeg = { source = "corp", version = "6.1" }"#);
        assert!(d
            .to_string()
            .contains("ffmpeg = { source = \"corp\", version = \"6.1\" }"));
    }

    #[test]
    fn unknown_source_lists_available() {
        let mut d = doc(BASE);
        let err = apply_add(&mut d, "ffmpeg", "6.1", Some("nope"))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "no source \"nope\" in [sources] (have: default)");
    }

    #[test]
    fn add_leaves_the_manifest_without_a_sources_stanza() {
        let mut d = doc("[package]\nname = \"a\"\nversion = \"0.1.0\"\nbase = \"alpine@3.20\"\n");
        apply_add(&mut d, "ffmpeg", "6.1", None).unwrap();
        let out = d.to_string();
        // The official registry is the resolver's fallback, so there is
        // nothing to write down — one less concept in a first manifest.
        assert!(!out.contains("[sources]"), "{out}");
        let m = ply_core::manifest::Manifest::parse(&out).unwrap();
        assert!(m.sources.is_empty());
        assert_eq!(m.dependencies.len(), 1);
    }

    #[test]
    fn dotted_names_are_quoted_keys() {
        let mut d = doc(BASE);
        let e = apply_add(&mut d, "py3.12-tools", "1.0", None).unwrap();
        assert_eq!(e.line, r#""py3.12-tools" = "1.0""#);
        assert!(d.to_string().contains("\"py3.12-tools\" = \"1.0\""));
    }

    #[test]
    fn parse_spec_splits_name_and_range() {
        assert_eq!(parse_spec("ffmpeg").unwrap(), ("ffmpeg".into(), None));
        assert_eq!(
            parse_spec("ffmpeg@6.1").unwrap(),
            ("ffmpeg".into(), Some("6.1".into()))
        );
        assert!(parse_spec("@6.1").is_err());
        assert!(parse_spec("ffmpeg@").is_err());
    }
}
