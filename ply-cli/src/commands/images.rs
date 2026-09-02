//! bundle / import / rebase / inspect — image transformation and
//! introspection commands.

use std::path::Path;

use anyhow::{Context, Result};

use crate::cli::{BundleArgs, ImportArgs, InspectArgs, RebaseArgs};
use crate::commands::build::human_size;

const LABEL_WIDTH: usize = 14;

/// One `label:    value` line, padded so every value in `render`'s output
/// starts in the same column regardless of the label's own length.
fn label_line(label: &str, value: &str) -> String {
    format!("{:<LABEL_WIDTH$}{value}", format!("{label}:"))
}

/// "—" for an empty list, else its items comma-joined.
fn list(values: &[String]) -> String {
    if values.is_empty() {
        "—".to_string()
    } else {
        values.join(", ")
    }
}

/// `volumes = { data = "/var/lib/x" }` and the table form `{ data = { path =
/// "…" } }` are the same declaration; both name a mount path.
fn volumes_of(manifest: &serde_json::Value) -> Vec<String> {
    let mut volumes: Vec<String> = manifest["volumes"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, v)| match v {
            serde_json::Value::String(path) => Some(path.clone()),
            other => other["path"].as_str().map(str::to_string),
        })
        .collect();
    volumes.sort();
    volumes.dedup();
    volumes
}

fn links_of(manifest: &serde_json::Value) -> Vec<String> {
    manifest["requests"]["links"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l.as_str().map(str::to_string))
        .collect()
}

/// `name range`, one per `[dependencies]` entry.
fn dependencies_of(manifest: &serde_json::Value) -> Vec<String> {
    manifest["dependencies"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, spec)| {
            let range = match spec {
                serde_json::Value::String(v) => v.clone(),
                other => other["version"].as_str().unwrap_or_default().to_string(),
            };
            format!("{name} {range}")
        })
        .collect()
}

/// The default `inspect` rendering: header, volumes/links/dependencies,
/// `[params]` (never a secret's value — `params_rows` returns `None` for
/// those by construction), and the two built-in/live footer lines. A pure
/// function of the record, so it needs no filesystem or network to test.
pub fn render(record: &ply_core::record::Record) -> String {
    let mut lines = vec![
        format!(
            "{} {}  {}  owner: {}",
            record.name,
            record.version,
            format!("{:?}", record.kind).to_lowercase(),
            record.owner.as_deref().unwrap_or("—")
        ),
        label_line("volumes", &list(&volumes_of(&record.manifest))),
        label_line("links", &list(&links_of(&record.manifest))),
        label_line("dependencies", &list(&dependencies_of(&record.manifest))),
    ];

    let rows = ply_core::record::params_rows(&record.manifest);
    if rows.is_empty() {
        lines.push(label_line("params", "none"));
    } else {
        lines.push(label_line(
            "params",
            &format!(
                "reference as {{{}.<name>}} from a stack; set with params = {{ <name> = \"…\" }}",
                record.name
            ),
        ));
        // Two columns of their own, padded independently of the `label:`
        // column above: name against every row (even a secret one, so its
        // own name still lines up), kind against only the rows that go on
        // to print a value (a secret row's kind is the last thing on its
        // line — padding it would only add invisible trailing spaces).
        let name_width = rows.iter().map(|(n, _, _)| n.len()).max().unwrap_or(0) + 2;
        let kind_width = rows
            .iter()
            .filter_map(|(_, k, v)| v.is_some().then_some(k.len()))
            .max()
            .unwrap_or(0)
            + 2;
        for (name, kind, value) in &rows {
            lines.push(match value {
                Some(v) => format!("  {name:<name_width$}{kind:<kind_width$}{v}"),
                None => format!("  {name:<name_width$}{kind}"),
            });
        }
    }

    lines.push(format!(
        "{}   (built-in, read-only)",
        label_line("facts", &ply_core::record::FACTS.join(" "))
    ));
    lines.push(format!(
        "{}   (after conditions only)",
        label_line("live", &ply_core::params::LIVE.join(" "))
    ));

    lines.join("\n")
}

/// Resolve `target` to a `Record` — the same shape `ply push` sends,
/// regardless of where it came from:
///
/// - a path that exists: `.img` (or any other existing file) →
///   `record_for_image`; a directory's `ply.toml` or a `.toml` file →
///   `record_for_toml`. A directory's `ply.toml` may be a stack or an
///   app/keg manifest — `record_for_toml` tells them apart itself (it runs
///   `stack::parse` first), so both read the same text the same way.
/// - otherwise, a registry ref (`postgres@17`, `owner/name@1.2`): resolved
///   and fetched exactly as `ply run` would (`fetch_app_image` against the
///   official run source — no new download path), then read back off the
///   image that lands in the store.
fn resolve_record(target: &str) -> Result<ply_core::record::Record> {
    let path = Path::new(target);
    if path.exists() {
        let (text_path, is_toml) = if path.is_dir() {
            (path.join("ply.toml"), true)
        } else {
            (
                path.to_path_buf(),
                path.extension().and_then(|e| e.to_str()) == Some("toml"),
            )
        };
        if is_toml {
            let text = std::fs::read_to_string(&text_path)
                .with_context(|| format!("reading {}", text_path.display()))?;
            return ply_core::record::record_for_toml(&text, &text_path)
                .with_context(|| format!("reading {}", text_path.display()));
        }
        return ply_core::record::record_for_image(path)
            .with_context(|| format!("reading {}", path.display()));
    }

    let (reference, want) = ply_core::catalog::parse_namespaced_ref(target).ok_or_else(|| {
        anyhow::anyhow!(
            "`{target}`: no such file or directory, and not a registry ref \
             (name, name@version, owner/name[@version])"
        )
    })?;
    let (image, _name, _digest) = ply_core::catalog::fetch_app_image(
        &reference,
        want.as_deref(),
        ply_core::catalog::OFFICIAL_RUN_SOURCE,
    )
    .with_context(|| format!("resolving {target}"))?;
    ply_core::record::record_for_image(&image)
        .with_context(|| format!("reading {}", image.display()))
}

/// `ply inspect`: read a package's record — from the registry, a local
/// image, a manifest, or a directory — and show it. `--json` prints the
/// record itself (identical shape to `ply push --dry-run`); `--manifest`
/// prints its embedded manifest text verbatim; the default is `render`.
pub fn inspect(args: InspectArgs) -> Result<()> {
    let record = resolve_record(&args.target)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&record)?);
        return Ok(());
    }
    if args.manifest {
        println!("{}", record.manifest_toml);
        return Ok(());
    }
    println!("{}", render(&record));
    Ok(())
}

pub fn bundle(args: BundleArgs) -> Result<()> {
    let outcome = ply_core::bundle::bundle(&args.image, &args.output, true)?;
    println!(
        "bundled {} ({}) — self-sufficient, zero fetches at run",
        outcome.image_path.display(),
        human_size(outcome.size_bytes)
    );
    println!("{}", outcome.digest);
    Ok(())
}

pub fn import(args: ImportArgs) -> Result<()> {
    let outcome = ply_core::oci::import(&args.source, &args.output)?;
    println!(
        "imported {} -> {} ({}, fat mode)",
        args.source,
        outcome.image_path.display(),
        human_size(outcome.size_bytes)
    );
    println!("{}", outcome.digest);
    Ok(())
}

pub fn rebase(args: RebaseArgs) -> Result<()> {
    let output = args.output.clone().unwrap_or_else(|| args.image.clone());
    let outcome =
        ply_core::rebase::rebase(&args.image, &args.runtime, &output, args.insecure_source)?;
    let (name, old, new) = &outcome.replaced;
    println!("rebased {name}: {old} -> {new}");
    println!("{} ({})", outcome.image_path.display(), outcome.digest);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The Task 7 `record.rs` fixture, copied here rather than shared: an app
    // with two secret params (one minted, one external) and two plain ones,
    // no volumes/links/dependencies declared.
    const PG: &str = r#"[package]
name = "postgres"
owner = "ply"
version = "17.10.7"
entrypoint = ["./run.sh"]
[params]
user = "postgres"
database = "postgres"
password = { secret = true }
key = { secret = true, external = true }
url = "postgres://{user}:{password}@{host}:{port}/{database}"
"#;

    #[test]
    fn render_prints_the_params_block_and_the_footer_verbatim() {
        let record = ply_core::record::record_for_toml(PG, Path::new("ply.toml")).unwrap();
        let out = render(&record);
        let expected_params = "params:       reference as {postgres.<name>} from a stack; set with params = { <name> = \"…\" }\n  database  default   postgres\n  key       secret, external\n  password  secret, minted\n  url       computed  postgres://{user}:{password}@{host}:{port}/{database}\n  user      default   postgres";
        assert!(
            out.contains(expected_params),
            "params block did not match:\n{out}"
        );
        let facts =
            "facts:        name version host port addr base_url scale arch image   (built-in, read-only)";
        let live = "live:         state instances started_at restarts   (after conditions only)";
        assert!(out.contains(facts), "{out}");
        assert!(out.contains(live), "{out}");
        // The header carries the record's own owner/name/version/type.
        assert!(
            out.starts_with("postgres 17.10.7  app  owner: ply\n"),
            "{out}"
        );
    }

    #[test]
    fn render_says_none_when_a_manifest_has_no_params() {
        let text = "[package]\nname = \"x\"\nversion = \"1.0.0\"\n";
        let record = ply_core::record::record_for_toml(text, Path::new("p")).unwrap();
        let out = render(&record);
        assert!(out.contains("params:       none"), "{out}");
        assert!(
            out.contains(
                "facts:        name version host port addr base_url scale arch image   (built-in, read-only)"
            ),
            "{out}"
        );
        assert!(
            out.contains(
                "live:         state instances started_at restarts   (after conditions only)"
            ),
            "{out}"
        );
        // No owner declared: the header falls back to the dash.
        assert!(out.starts_with("x 1.0.0  layer  owner: —\n"), "{out}");
    }

    #[test]
    fn a_toml_file_target_resolves_via_record_for_toml() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("postgres.toml");
        std::fs::write(&toml_path, PG).unwrap();
        let record = resolve_record(toml_path.to_str().unwrap()).unwrap();
        assert_eq!(record.name, "postgres");
        assert_eq!(record.manifest_toml, PG);
    }

    #[test]
    fn a_directory_target_reads_its_ply_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ply.toml"), PG).unwrap();
        let record = resolve_record(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(record.name, "postgres");
        assert_eq!(record.owner.as_deref(), Some("ply"));
    }

    #[test]
    fn a_target_that_is_neither_a_path_nor_a_ref_grammar_is_a_clear_error() {
        let err = resolve_record("./nonexistent-dir-xyz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("nonexistent-dir-xyz"), "{err}");
    }
}
