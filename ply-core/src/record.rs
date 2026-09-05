//! The record: what `ply push` sends and `ply inspect` reads. The manifest
//! embedded inside an image (or a not-yet-built manifest text) IS the record
//! — a self-contained TOML with its key-for-key JSON rendering, not a
//! separately-derived summary a server has to trust on faith.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::catalog::PackageKind;
use crate::manifest::Manifest;
use crate::{stack, Error, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub arch: String,
    pub src: String,
    pub sha256: String,
    pub bytes: u64,
    pub verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub name: String,
    pub version: String,
    #[serde(rename = "type")]
    pub kind: PackageKind,
    pub manifest_toml: String,
    pub manifest: serde_json::Value,
    pub artifacts: Vec<Artifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_by: Option<String>,
}

/// The record for a built image: manifest text read from `/.manifest.toml`
/// INSIDE the image (what the artifact really contains), JSON rendered from
/// it.
pub fn record_for_image(image: &Path) -> Result<Record> {
    let bytes = crate::image::read::read_embedded(image, crate::image::read::MANIFEST_PATH)?
        .ok_or_else(|| {
            Error::Runtime(format!(
                "{}: no {} inside — not a ply image",
                image.display(),
                crate::image::read::MANIFEST_PATH
            ))
        })?;
    let text = String::from_utf8(bytes).map_err(|e| {
        Error::Runtime(format!(
            "{}: embedded manifest is not valid UTF-8: {e}",
            image.display()
        ))
    })?;
    record_for_toml(&text, image)
}

/// `toml::Value` → `serde_json::Value`, key for key. `serde_json::to_value`
/// looks like the obvious way to do this but is NOT equivalent: `toml`'s
/// `Datetime` serializes through serde as a special-cased map
/// (`{"$__toml_private_datetime": "…"}`) so round-tripping tools recognize
/// it, and that internal wrapper leaks straight into the JSON — an invented
/// key nothing declared. So this walks the value by hand instead of going
/// through serde. `serde_json::Map` here is a `BTreeMap` (the workspace
/// doesn't enable serde_json's `preserve_order` feature) — object keys come
/// out alphabetized, not in source order, which is fine: nothing reads
/// `manifest` positionally.
fn toml_to_json(v: &toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            // NaN/inf have no JSON number form — fall back to the string
            // rather than silently dropping the field.
            .unwrap_or_else(|| serde_json::Value::String(f.to_string())),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        // Unreachable: `record_for_toml` refuses a document containing a
        // datetime before it ever renders one (see `first_datetime`). Kept
        // so the match stays total, and harmless if it ever runs.
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
        toml::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(toml_to_json).collect())
        }
        toml::Value::Table(table) => serde_json::Value::Object(
            table
                .iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}

/// The dotted path of the first TOML datetime anywhere in `v`, if any.
/// `[a] b = 2024-01-02` → `a.b`; `[[a]] b = …` → `a[0].b`.
fn first_datetime(v: &toml::Value, path: &str) -> Option<String> {
    match v {
        toml::Value::Datetime(_) => Some(path.to_string()),
        toml::Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, item)| first_datetime(item, &format!("{path}[{i}]"))),
        toml::Value::Table(table) => table.iter().find_map(|(k, item)| {
            let child = if path.is_empty() {
                k.clone()
            } else {
                format!("{path}.{k}")
            };
            first_datetime(item, &child)
        }),
        _ => None,
    }
}

/// The record for a manifest text that is not (yet) an image: a stack file,
/// or a dir's ply.toml for `inspect`.
pub fn record_for_toml(text: &str, path: &Path) -> Result<Record> {
    let doc: toml::Value = text.parse().map_err(|e| {
        Error::Manifest(format!(
            "{}: {e} — not valid TOML; fix the syntax and try again",
            path.display()
        ))
    })?;
    // A TOML datetime has no agreed JSON rendering, and the record carries
    // BOTH the text and its JSON: this renders `2024-01-02T03:04:05Z`, while
    // the registry re-parses `manifest_toml` with a JS `Date` and renders
    // `2024-01-02T03:04:05.000Z`. The two would never match, and the publish
    // would die on "manifest does not match manifest_toml" — an error that
    // names nothing the author can fix. Refuse here, naming the key and the
    // remedy, rather than let an unpublishable image get built.
    if let Some(key) = first_datetime(&doc, "") {
        return Err(Error::Manifest(format!(
            "{}: TOML datetimes are not supported in a published manifest (key `{key}`) — quote it as a string",
            path.display()
        )));
    }
    let manifest = toml_to_json(&doc);

    // A stack file is `[[app]]` wiring, not a package — `stack::parse`
    // already knows the shape (and its errors already carry a good remedy).
    if let Some(stack) = stack::parse(text, path)? {
        return Ok(Record {
            owner: stack.owner,
            name: stack.name.unwrap_or_default(),
            version: stack.version.unwrap_or_default(),
            kind: PackageKind::Stack,
            manifest_toml: text.to_string(),
            manifest,
            artifacts: Vec::new(),
            pushed_at: None,
            published_by: None,
        });
    }

    // Not a stack: an app or a layer manifest, and it must be a genuinely
    // valid one — a manifest the CLI itself would reject (an unresolved
    // param hole, a `deny_unknown_fields` typo an old binary doesn't
    // recognize) is never a record. This is the forward-compatibility gate:
    // fail loudly here, naming the path, rather than publish something
    // nothing downstream can actually read. `name`/`version`/`owner`/`kind`
    // all come from the typed, validated `Manifest` — never a raw-TOML
    // fallback.
    let m = Manifest::parse(text).map_err(|e| match e {
        Error::Manifest(msg) => Error::Manifest(format!("{}: {msg}", path.display())),
        other => other,
    })?;
    let kind = if m.package.entrypoint.is_some() {
        PackageKind::App
    } else {
        PackageKind::Layer
    };

    Ok(Record {
        owner: m.package.owner,
        name: m.package.name,
        version: m.package.version.to_string(),
        kind,
        manifest_toml: text.to_string(),
        manifest,
        artifacts: Vec::new(),
        pushed_at: None,
        published_by: None,
    })
}

/// `{version}` / `{arch}` in a `--src` template.
pub fn expand_src(template: &str, version: &str, arch: &str) -> String {
    template
        .replace("{version}", version)
        .replace("{arch}", arch)
}

/// Display rows for `[params]`: (name, kind, value) where kind ∈ "default" |
/// "computed" | "secret, minted" | "secret, external". Secret rows never
/// carry a value — there is nothing to show, minted or not.
pub fn params_rows(manifest: &serde_json::Value) -> Vec<(String, &'static str, Option<String>)> {
    let mut rows: Vec<(String, &'static str, Option<String>)> = manifest
        .get("params")
        .and_then(|p| p.as_object())
        .into_iter()
        .flatten()
        .map(|(name, value)| {
            let (kind, val): (&'static str, Option<String>) = match value {
                serde_json::Value::Object(map)
                    if map.get("secret") == Some(&serde_json::Value::Bool(true)) =>
                {
                    let external = map.get("external") == Some(&serde_json::Value::Bool(true));
                    (
                        if external {
                            "secret, external"
                        } else {
                            "secret, minted"
                        },
                        None,
                    )
                }
                serde_json::Value::String(s) if s.contains('{') => ("computed", Some(s.clone())),
                serde_json::Value::String(s) => ("default", Some(s.clone())),
                other => ("default", Some(other.to_string())),
            };
            (name.clone(), kind, val)
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// The `[network] egress` list as written, from a record's manifest JSON.
pub fn egress_entries(manifest: &serde_json::Value) -> Option<Vec<String>> {
    manifest.get("network")?.get("egress")?.as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    })
}

/// Built-in facts and live names, for `inspect`'s footer.
pub const FACTS: &[&str] = &[
    "name", "version", "host", "port", "addr", "base_url", "scale", "arch", "image",
];

#[cfg(test)]
mod tests {
    use super::*;
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
    fn a_toml_becomes_a_record_key_for_key() {
        let r = record_for_toml(PG, Path::new("ply.toml")).unwrap();
        assert_eq!(
            (r.owner.as_deref(), r.name.as_str(), r.version.as_str()),
            (Some("ply"), "postgres", "17.10.7")
        );
        assert_eq!(r.kind, crate::catalog::PackageKind::App);
        assert_eq!(
            r.manifest["params"]["password"]["secret"],
            serde_json::json!(true)
        );
        assert_eq!(
            r.manifest["package"]["entrypoint"][0],
            serde_json::json!("./run.sh")
        );
        assert!(r.artifacts.is_empty());
        assert_eq!(r.manifest_toml, PG);
    }
    #[test]
    fn a_layer_and_a_stack_are_classified() {
        assert_eq!(
            record_for_toml(
                "[package]\nname = \"x\"\nversion = \"1.0.0\"\n",
                Path::new("p")
            )
            .unwrap()
            .kind,
            crate::catalog::PackageKind::Layer
        );
        let s = record_for_toml(
            "[stack]\nname = \"todos\"\nversion = \"0.1.0\"\n[[app]]\nrun = \"postgres@17\"\n",
            Path::new("stack.toml"),
        )
        .unwrap();
        assert_eq!(s.kind, crate::catalog::PackageKind::Stack);
        assert_eq!(s.name, "todos");
    }
    #[test]
    fn an_image_record_reads_the_embedded_manifest() {
        // build a tiny image the way manifest/read tests do (see image::squashfs tests for the helper that packs `/.manifest.toml`)
        let td = tempfile::tempdir().unwrap();
        let img = crate::image::squashfs::test_image_with_manifest(td.path(), PG); // add this test helper if absent (pub(crate) under cfg(test))
        let r = record_for_image(&img).unwrap();
        assert_eq!(r.name, "postgres");
        assert_eq!(r.manifest_toml, PG);
    }
    #[test]
    fn src_templates_expand_version_and_arch() {
        assert_eq!(
            expand_src("https://h/pg-{version}-linux-{arch}.img", "17.10.7", "x64"),
            "https://h/pg-17.10.7-linux-x64.img"
        );
        assert_eq!(
            expand_src("https://h/fixed.img", "1", "arm64"),
            "https://h/fixed.img"
        );
    }
    #[test]
    fn params_rows_never_carry_a_secret_value() {
        let r = record_for_toml(PG, Path::new("p")).unwrap();
        let rows = params_rows(&r.manifest);
        assert_eq!(
            rows.iter().find(|r| r.0 == "password").unwrap().1,
            "secret, minted"
        );
        assert_eq!(
            rows.iter().find(|r| r.0 == "key").unwrap().1,
            "secret, external"
        );
        assert_eq!(rows.iter().find(|r| r.0 == "url").unwrap().1, "computed");
        assert!(rows
            .iter()
            .all(|r| r.1.starts_with("secret") == r.2.is_none()));
    }
    #[test]
    fn an_invalid_manifest_is_not_a_record() {
        // The old PG fixture, before `database` was declared: `url`'s
        // `{database}` hole names no declared param, so `Manifest::parse`
        // rejects it — and a manifest the CLI rejects is never a record.
        let bad = r#"[package]
name = "postgres"
owner = "ply"
version = "17.10.7"
entrypoint = ["./run.sh"]
[params]
user = "postgres"
password = { secret = true }
key = { secret = true, external = true }
url = "postgres://{user}:{password}@{host}:{port}/{database}"
"#;
        let err = record_for_toml(bad, Path::new("ply.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("ply.toml"), "{err}");
    }
    #[test]
    fn a_datetime_is_refused_with_a_remedy() {
        // A stack file's top level is not `deny_unknown_fields`, so an extra
        // table holding a bare TOML datetime used to ride through as a
        // string. It cannot: the registry re-parses `manifest_toml` and
        // renders a datetime as `2024-01-02T03:04:05.000Z` (JS `Date`),
        // while this renders `2024-01-02T03:04:05Z` — the record and its
        // text would never match, and the publish would fail with "manifest
        // does not match manifest_toml", which names nothing actionable.
        // Refuse at the source instead, naming the key and the fix.
        let text = "[stack]\nname = \"todos\"\nversion = \"0.1.0\"\n\n[[app]]\nrun = \"postgres@17\"\n\n[extra]\nbuilt = 2024-01-02T03:04:05Z\n";
        let err = record_for_toml(text, Path::new("stack.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("stack.toml"), "{err}");
        assert!(err.contains("`extra.built`"), "{err}");
        assert!(err.contains("quote it as a string"), "{err}");

        // Nested in an array of tables, too — the walk is the whole document.
        let nested = "[package]\nname = \"a\"\nversion = \"1.0.0\"\n\n[[extra]]\nat = 2024-01-02\n";
        let err = record_for_toml(nested, Path::new("ply.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`extra[0].at`"), "{err}");

        // The same date QUOTED is an ordinary string and publishes fine.
        let quoted = "[stack]\nname = \"todos\"\nversion = \"0.1.0\"\n\n[[app]]\nrun = \"postgres@17\"\n\n[extra]\nbuilt = \"2024-01-02T03:04:05Z\"\n";
        let r = record_for_toml(quoted, Path::new("stack.toml")).unwrap();
        assert_eq!(
            r.manifest["extra"]["built"],
            serde_json::json!("2024-01-02T03:04:05Z")
        );

        fn no_dollar_keys(v: &serde_json::Value) -> bool {
            match v {
                serde_json::Value::Object(map) => map
                    .iter()
                    .all(|(k, v)| !k.starts_with('$') && no_dollar_keys(v)),
                serde_json::Value::Array(items) => items.iter().all(no_dollar_keys),
                _ => true,
            }
        }
        assert!(no_dollar_keys(&r.manifest), "{}", r.manifest);
    }
}
