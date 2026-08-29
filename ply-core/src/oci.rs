//! `ply import docker://image[:tag]` — the one-way ecosystem bridge.
//! Pulls an OCI image (registry v2 API), flattens its layers (whiteout-aware)
//! and writes a fat ply image.

use std::io::Read;
use std::path::{Path, PathBuf};

use semver::Version;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::image::name::{Arch, ImageName, Os};
use crate::image::read::MANIFEST_PATH;
use crate::image::squashfs::{write_image, ExtraFile, TreeSource};
use crate::manifest::{Manifest, Package};

pub struct ImportOutcome {
    pub image_path: PathBuf,
    pub image_name: ImageName,
    pub digest: String,
    pub size_bytes: u64,
}

struct ImageRef {
    registry: String,
    repository: String,
    tag: String,
}

fn parse_ref(spec: &str) -> Result<ImageRef> {
    let rest = spec.strip_prefix("docker://").ok_or_else(|| {
        Error::Source(format!(
            "import source `{spec}`: expected docker://name[:tag]"
        ))
    })?;
    let (name, tag) = match rest.rsplit_once(':') {
        Some((n, t)) if !t.contains('/') => (n.to_string(), t.to_string()),
        _ => (rest.to_string(), "latest".to_string()),
    };
    // bare names (nginx) → docker.io library/; host-qualified pass through
    let (registry, repository) = match name.split_once('/') {
        Some((host, path)) if host.contains('.') || host.contains(':') => {
            (host.to_string(), path.to_string())
        }
        Some(_) => ("registry-1.docker.io".to_string(), name.clone()),
        None => (
            "registry-1.docker.io".to_string(),
            format!("library/{name}"),
        ),
    };
    Ok(ImageRef {
        registry,
        repository,
        tag,
    })
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

#[derive(Deserialize)]
struct ManifestDoc {
    #[serde(default)]
    manifests: Vec<PlatformManifest>, // present in index/list
    #[serde(default)]
    layers: Vec<Descriptor>, // present in image manifest
    config: Option<Descriptor>,
}

#[derive(Deserialize)]
struct PlatformManifest {
    digest: String,
    platform: Option<Platform>,
}

#[derive(Deserialize)]
struct Platform {
    architecture: String,
    os: String,
}

#[derive(Deserialize)]
struct Descriptor {
    digest: String,
    #[serde(rename = "mediaType", default)]
    media_type: String,
}

#[derive(Deserialize)]
struct ConfigDoc {
    config: Option<OciConfig>,
}

#[derive(Deserialize, Default)]
struct OciConfig {
    #[serde(rename = "Env", default)]
    env: Vec<String>,
    #[serde(rename = "Entrypoint", default)]
    entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd", default)]
    cmd: Option<Vec<String>>,
    #[serde(rename = "ExposedPorts", default)]
    exposed_ports: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    /// The image's `WORKDIR`. Entrypoints that operate on `.` depend on it —
    /// dropping it starts them at `/` instead.
    #[serde(rename = "WorkingDir", default)]
    working_dir: Option<String>,
    /// The image's `USER`. Daemons that refuse to run as root depend on it —
    /// memcached exits 64 rather than start.
    #[serde(rename = "User", default)]
    user: Option<String>,
    /// How the image asks to be shut down: nginx "SIGQUIT", httpd "SIGWINCH".
    #[serde(rename = "StopSignal", default)]
    stop_signal: Option<String>,
    /// Paths the image declares must be writable (Docker `VOLUME`). Without
    /// a real volume behind them, an app that runs as a non-root user can't
    /// write its data dir (n8n's /home/node/.n8n) and crashes. We turn each
    /// into a ply volume so the runtime creates and chowns it.
    #[serde(rename = "Volumes", default)]
    volumes: Option<std::collections::BTreeMap<String, serde_json::Value>>,
}

const ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.oci.image.index.v1+json";

struct Client {
    registry: String,
    repository: String,
    token: Option<String>,
}

impl Client {
    fn get(&mut self, path: &str, accept: &str) -> Result<ureq::http::Response<ureq::Body>> {
        let url = format!("https://{}/v2/{}/{path}", self.registry, self.repository);
        let attempt = |token: &Option<String>| {
            let mut request = ureq::get(&url).header("Accept", accept);
            if let Some(token) = token {
                request = request.header("Authorization", &format!("Bearer {token}"));
            }
            request.call()
        };
        match attempt(&self.token) {
            Ok(response) => Ok(response),
            Err(ureq::Error::StatusCode(401)) if self.token.is_none() => {
                self.token = Some(self.fetch_token()?);
                attempt(&self.token).map_err(|e| Error::Source(format!("{url}: {e}")))
            }
            Err(e) => Err(Error::Source(format!("{url}: {e}"))),
        }
    }

    fn fetch_token(&self) -> Result<String> {
        // docker.io convention; other registries advertise via WWW-Authenticate
        // but the big ones all follow the same token endpoint shape.
        let url = if self.registry == "registry-1.docker.io" {
            format!(
                "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
                self.repository
            )
        } else {
            format!(
                "https://{}/token?scope=repository:{}:pull",
                self.registry, self.repository
            )
        };
        let mut response = ureq::get(&url)
            .call()
            .map_err(|e| Error::Source(format!("token {url}: {e}")))?;
        let body: TokenResponse = serde_json::from_reader(response.body_mut().as_reader())
            .map_err(|e| Error::Source(format!("token {url}: {e}")))?;
        Ok(body.token)
    }
}

/// The image's `WORKDIR` as a ply `[package] workdir`. An absent or empty
/// `WorkingDir` is how OCI spells "no WORKDIR" — both become None so the
/// runtime falls back to the app prefix instead of chdir'ing to "". A
/// relative value is unusable inside the container and is dropped too.
fn workdir_from_config(config: &OciConfig) -> Option<String> {
    config
        .working_dir
        .as_deref()
        .map(str::trim)
        .filter(|d| d.starts_with('/'))
        .map(str::to_string)
}

/// The image's `USER` as a ply `[package] user` ("name:uid:gid").
///
/// OCI allows `memcache`, `11`, `11:11` or `memcache:memcache`, and names
/// only mean anything against the image's OWN account files — so resolve
/// against the extracted rootfs, not the host's. Unresolvable names are
/// dropped rather than guessed: running as root is at least a state the
/// operator can see, where a wrong uid silently writes files nobody owns.
fn user_from_config(config: &OciConfig, rootfs: &Path) -> Option<String> {
    let spec = config
        .user
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())?;
    let (user_part, group_part) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };

    // `<file>` line whose field 0 matches `key`, or whose id field matches it.
    let lookup = |file: &str, key: &str| -> Option<(String, u32)> {
        let text = std::fs::read_to_string(rootfs.join(file)).ok()?;
        for line in text.lines() {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() < 3 || (f[0] != key && f[2] != key) {
                continue;
            }
            // a malformed line is skipped, never fatal to the whole search
            if let Ok(id) = f[2].parse::<u32>() {
                return Some((f[0].to_string(), id));
            }
        }
        None
    };

    let (name, uid) = match user_part.parse::<u32>() {
        // numeric: keep the id, borrow the name if the image knows one
        Ok(uid) => (
            lookup("etc/passwd", user_part)
                .map(|(n, _)| n)
                .unwrap_or_else(|| format!("uid{uid}")),
            uid,
        ),
        Err(_) => lookup("etc/passwd", user_part)?,
    };

    let gid = match group_part {
        Some(g) => match g.parse::<u32>() {
            Ok(gid) => gid,
            Err(_) => lookup("etc/group", g).map(|(_, gid)| gid)?,
        },
        // no group given: the account's primary gid, else mirror the uid
        None => std::fs::read_to_string(rootfs.join("etc/passwd"))
            .ok()
            .and_then(|text| {
                text.lines()
                    .map(|l| l.split(':').collect::<Vec<_>>())
                    .find(|f| f.len() > 3 && f[0] == name)
                    .and_then(|f| f[3].parse::<u32>().ok())
            })
            .unwrap_or(uid),
    };
    Some(format!("{name}:{uid}:{gid}"))
}

/// `ply run docker://postgres:16` — import on demand, cached by reference.
///
/// A path is returned unchanged; a `docker://` reference is imported once into
/// the data dir and reused after that, so the second run starts instantly and
/// offline. This exists because authoring a manifest is the right amount of
/// work for *your* service and far too much for a database you need for an
/// afternoon — the two cases deserve different front doors.
///
/// The cache is keyed by the reference as written, so a moving tag (`:16`)
/// keeps resolving to the image first pulled. That is deliberate: a local dev
/// database should not change under you because upstream moved a tag. Pass
/// `pull` to refresh it.
pub fn ensure_local(spec: &str, pull: bool) -> Result<PathBuf> {
    let Some(reference) = spec.strip_prefix("docker://") else {
        return Ok(PathBuf::from(spec));
    };
    let slug: String = reference
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = crate::paths::data_dir().join("imports");
    let path = dir.join(format!("{slug}.img"));

    if path.exists() && !pull {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
        path: dir.clone(),
        source,
    })?;
    eprintln!("ply: importing {spec} (cached as {})", path.display());
    import(spec, &path)?;
    Ok(path)
}

pub fn import(spec: &str, output: &Path) -> Result<ImportOutcome> {
    let image_ref = parse_ref(spec)?;
    let mut client = Client {
        registry: image_ref.registry.clone(),
        repository: image_ref.repository.clone(),
        token: None,
    };

    // Manifest (resolving a multi-arch index to this host's platform).
    let mut response = client.get(&format!("manifests/{}", image_ref.tag), ACCEPT)?;
    let mut doc: ManifestDoc = serde_json::from_reader(response.body_mut().as_reader())
        .map_err(|e| Error::Source(format!("manifest: {e}")))?;
    if !doc.manifests.is_empty() {
        let want_arch = match Arch::host() {
            Arch::X64 => "amd64",
            Arch::Arm64 => "arm64",
        };
        let platform = doc
            .manifests
            .iter()
            .find(|m| {
                m.platform
                    .as_ref()
                    .map(|p| p.os == "linux" && p.architecture == want_arch)
                    .unwrap_or(false)
            })
            .ok_or_else(|| Error::Source(format!("no linux/{want_arch} manifest in {spec}")))?;
        let digest = platform.digest.clone();
        let mut response = client.get(&format!("manifests/{digest}"), ACCEPT)?;
        doc = serde_json::from_reader(response.body_mut().as_reader())
            .map_err(|e| Error::Source(format!("platform manifest: {e}")))?;
    }
    if doc.layers.is_empty() {
        return Err(Error::Source(format!("{spec}: manifest has no layers")));
    }

    // Config → entrypoint/env/ports.
    let config: OciConfig = match &doc.config {
        Some(descriptor) => {
            let mut response = client.get(&format!("blobs/{}", descriptor.digest), "*/*")?;
            let parsed: ConfigDoc = serde_json::from_reader(response.body_mut().as_reader())
                .map_err(|e| Error::Source(format!("config blob: {e}")))?;
            parsed.config.unwrap_or_default()
        }
        None => OciConfig::default(),
    };

    // Layers → flattened rootfs (whiteout-aware).
    let tmp = tempfile::tempdir().map_err(|source| Error::Io {
        path: PathBuf::from("tempdir"),
        source,
    })?;
    let rootfs = tmp.path().join("rootfs");
    std::fs::create_dir_all(&rootfs).map_err(|source| Error::Io {
        path: rootfs.clone(),
        source,
    })?;
    let mut deferred_dir_modes: std::collections::BTreeMap<PathBuf, u32> =
        std::collections::BTreeMap::new();
    for (i, layer) in doc.layers.iter().enumerate() {
        eprintln!(
            "ply: layer {}/{} {}",
            i + 1,
            doc.layers.len(),
            &layer.digest[..19.min(layer.digest.len())]
        );
        let response = client.get(&format!("blobs/{}", layer.digest), "*/*")?;
        let reader = response.into_body().into_reader();
        // Modern registries ship layers gzip'd, zstd'd, or bare. Pick the
        // decoder by media type; an empty type means the legacy gzip default.
        let comp = if layer.media_type.ends_with("zstd") {
            LayerComp::Zstd
        } else if layer.media_type.ends_with("gzip") || layer.media_type.is_empty() {
            LayerComp::Gzip
        } else {
            LayerComp::None
        };
        apply_layer_tar(reader, comp, &rootfs, &mut deferred_dir_modes)?;
    }
    restore_dir_modes(&deferred_dir_modes);

    // Synthesized ply manifest.
    let name = image_ref
        .repository
        .rsplit('/')
        .next()
        .unwrap_or("imported")
        .replace(['_', '.'], "-");
    let version = Version::parse(&image_ref.tag).unwrap_or_else(|_| Version::new(0, 0, 0));
    // Read before the Entrypoint/Cmd moves below partially consume `config`.
    let workdir = workdir_from_config(&config);
    let run_user = user_from_config(&config, &rootfs);
    let stop_signal = config
        .stop_signal
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut entrypoint: Vec<String> = config.entrypoint.unwrap_or_default();
    entrypoint.extend(config.cmd.unwrap_or_default());
    if entrypoint.is_empty() {
        return Err(Error::Source(format!(
            "{spec}: image has no Entrypoint/Cmd — nothing to run"
        )));
    }
    let mut env = std::collections::BTreeMap::new();
    for pair in &config.env {
        if let Some((k, v)) = pair.split_once('=') {
            env.insert(k.to_string(), v.to_string());
        }
    }
    let mut ports = std::collections::BTreeMap::new();
    if let Some(exposed) = &config.exposed_ports {
        for (i, key) in exposed.keys().enumerate() {
            if let Some(port) = key.split('/').next().and_then(|p| p.parse::<u16>().ok()) {
                ports.insert(
                    if i == 0 {
                        "main".into()
                    } else {
                        format!("p{port}")
                    },
                    port,
                );
            }
        }
    }
    // Declared VOLUMEs become ply volumes: named from the path, created and
    // chowned to the app's user at run time (the fix for imported apps that
    // write a data dir as a non-root user).
    let mut volumes = std::collections::BTreeMap::new();
    if let Some(declared) = &config.volumes {
        for (i, path) in declared.keys().enumerate() {
            let base = path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .trim_start_matches('.');
            let mut vname: String = base
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            vname = vname.trim_matches('-').to_string();
            if vname.is_empty() {
                vname = format!("vol{i}");
            }
            volumes
                .entry(vname)
                .or_insert_with(|| crate::manifest::Volume {
                    path: path.clone(),
                    scope: "instance".into(),
                    ephemeral: false,
                });
        }
    }
    let manifest = Manifest {
        package: Package {
            name: name.clone(),
            version: version.clone(),
            entrypoint: Some(entrypoint),
            base: Default::default(),
            provides_abi: None,
            user: run_user,
            stop_signal,
            workdir,
            // Official images are built against Docker's capability set:
            // their entrypoints chown the data dir and gosu down to a service
            // user. ply-native packages never need this — [package] user does
            // the same job from the parent, before rights stripping.
            capabilities: Some(crate::manifest::Capabilities::Preset("oci".into())),
            include: vec![],
            isolation: "ns".into(),
        },
        dependencies: Default::default(),
        env,
        ports,
        volumes,
        resources: None,
        requires: None,
        health: None,
        restart: None,
        layer: None,
        sources: Default::default(),
        requests: None,
    };

    let image_name = ImageName::new(&name, version, Os::Linux, Arch::host())?;
    let trees = [TreeSource {
        dir: &rootfs,
        prefix: "",
        filter: None,
    }];
    let extra = [ExtraFile {
        path: MANIFEST_PATH.into(),
        bytes: manifest.to_toml()?.into_bytes(),
        mode: 0o444,
    }];
    let tmp_out = output.with_extension("img.tmp");
    write_image(&trees, &extra, &tmp_out)?;
    std::fs::rename(&tmp_out, output).map_err(|source| Error::Io {
        path: output.to_path_buf(),
        source,
    })?;

    Ok(ImportOutcome {
        image_path: output.to_path_buf(),
        image_name,
        digest: crate::digest::sha256_file(output)?,
        size_bytes: std::fs::metadata(output)
            .map_err(|source| Error::Io {
                path: output.to_path_buf(),
                source,
            })?
            .len(),
    })
}

/// Apply one OCI layer tar: regular extract + whiteout handling
/// (`.wh.<name>` deletes, `.wh..wh..opq` empties the directory).
/// Restore deferred directory modes, deepest first. BTreeMap order puts a
/// parent before its children, so reversing walks children first — a dir is
/// only sealed once nothing else needs to be written inside it.
fn restore_dir_modes(modes: &std::collections::BTreeMap<PathBuf, u32>) {
    use std::os::unix::fs::PermissionsExt;
    for (dir, mode) in modes.iter().rev() {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(*mode));
    }
}

/// Apply one layer tar into `rootfs`.
///
/// Directory modes are NOT applied as encountered. An image whose tar creates
/// a directory read-only (mysql:8 ships `etc/pki/ca-trust/.../directory-hash`
/// at 0555) would otherwise refuse every later entry inside it — invisibly to
/// root, which has CAP_DAC_OVERRIDE, and fatally to everyone else. Modes are
/// collected in `deferred` and restored by the caller once EVERY layer has
/// been applied, since a later layer may still add files to an earlier
/// layer's directory.
#[derive(Clone, Copy)]
enum LayerComp {
    Gzip,
    Zstd,
    None,
}

fn apply_layer_tar<'a>(
    reader: impl Read + 'a,
    comp: LayerComp,
    rootfs: &Path,
    deferred: &mut std::collections::BTreeMap<PathBuf, u32>,
) -> Result<()> {
    let reader: Box<dyn Read + 'a> = match comp {
        LayerComp::Gzip => Box::new(flate2::read::GzDecoder::new(reader)),
        LayerComp::Zstd => Box::new(
            zstd::stream::read::Decoder::new(reader)
                .map_err(|e| Error::Runtime(format!("zstd layer decode: {e}")))?,
        ),
        LayerComp::None => Box::new(reader),
    };
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_unpack_xattrs(false);

    for entry in archive
        .entries()
        .map_err(|e| Error::Source(format!("layer tar: {e}")))?
    {
        let mut entry = entry.map_err(|e| Error::Source(format!("layer tar entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| Error::Source(format!("layer tar path: {e}")))?
            .into_owned();
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        if file_name == ".wh..wh..opq" {
            if let Some(parent) = path.parent() {
                let dir = rootfs.join(parent);
                if dir.exists() {
                    let _ = std::fs::remove_dir_all(&dir);
                    let _ = std::fs::create_dir_all(&dir);
                }
            }
            continue;
        }
        if let Some(hidden) = file_name.strip_prefix(".wh.") {
            if let Some(parent) = path.parent() {
                let victim = rootfs.join(parent).join(hidden);
                if victim.is_dir() {
                    let _ = std::fs::remove_dir_all(&victim);
                } else {
                    let _ = std::fs::remove_file(&victim);
                }
            }
            continue;
        }
        match entry.header().entry_type() {
            tar::EntryType::Char | tar::EntryType::Block | tar::EntryType::Fifo => continue,
            _ => {}
        }
        // conflicting file types from earlier layers lose
        let target = rootfs.join(&path);
        if target.exists() && !target.is_dir() {
            let _ = std::fs::remove_file(&target);
        }
        let is_dir = entry.header().entry_type() == tar::EntryType::Directory;
        let mode = entry.header().mode().unwrap_or(0o755) & 0o7777;
        entry
            .unpack_in(rootfs)
            .map_err(|e| Error::Source(format!("unpack {}: {e}", path.display())))?;
        if is_dir {
            use std::os::unix::fs::PermissionsExt;
            // stays traversable+writable until every layer is in
            let _ =
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode | 0o700));
            deferred.insert(target, mode);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: &str) -> OciConfig {
        serde_json::from_str(json).expect("config parses")
    }

    #[test]
    fn working_dir_becomes_the_package_workdir() {
        // redis:7-alpine declares WORKDIR /data; without it the entrypoint's
        // `find . -exec chown redis {} +` walks the whole rootfs from /.
        let cfg = config(r#"{"WorkingDir":"/data","Cmd":["redis-server"]}"#);
        assert_eq!(workdir_from_config(&cfg).as_deref(), Some("/data"));
    }

    #[test]
    fn absent_empty_or_relative_working_dir_is_none() {
        for json in [
            r#"{"Cmd":["sh"]}"#,
            r#"{"WorkingDir":"","Cmd":["sh"]}"#,
            r#"{"WorkingDir":"   ","Cmd":["sh"]}"#,
            r#"{"WorkingDir":"app","Cmd":["sh"]}"#,
        ] {
            assert_eq!(
                workdir_from_config(&config(json)),
                None,
                "should fall back to the app prefix: {json}"
            );
        }
    }

    #[test]
    fn config_still_parses_the_fields_import_already_relied_on() {
        let cfg = config(
            r#"{"Env":["PATH=/usr/bin"],"Entrypoint":["docker-entrypoint.sh"],
                "Cmd":["redis-server"],"ExposedPorts":{"6379/tcp":{}},"WorkingDir":"/data"}"#,
        );
        assert_eq!(cfg.env, vec!["PATH=/usr/bin"]);
        assert_eq!(
            cfg.entrypoint.as_deref(),
            Some(&["docker-entrypoint.sh".to_string()][..])
        );
        assert_eq!(cfg.cmd.as_deref(), Some(&["redis-server".to_string()][..]));
        assert!(cfg.exposed_ports.expect("ports").contains_key("6379/tcp"));
    }
}

#[cfg(test)]
mod ensure_local_tests {
    use super::*;

    #[test]
    fn a_plain_path_is_returned_untouched() {
        // no network, no cache, no surprise — the common case must stay inert
        assert_eq!(
            ensure_local("./myapp-1.0.0-linux-x64.img", false).unwrap(),
            PathBuf::from("./myapp-1.0.0-linux-x64.img")
        );
        assert_eq!(
            ensure_local("/srv/app/current.img", true).unwrap(),
            PathBuf::from("/srv/app/current.img")
        );
    }

    #[test]
    fn a_cached_reference_is_reused_without_touching_the_network() {
        // the point of the cache: the second `ply run docker://…` is instant
        // and works offline. If this ever hits the network the test hangs
        // rather than passing, which is the failure we want to notice.
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let dir = crate::paths::data_dir().join("imports");
        std::fs::create_dir_all(&dir).unwrap();
        let cached = dir.join("postgres-16.img");
        std::fs::write(&cached, b"not really an image").unwrap();

        assert_eq!(ensure_local("docker://postgres:16", false).unwrap(), cached);
    }

    #[test]
    fn references_map_to_distinct_cache_entries() {
        // a tag, a registry host and a path must not collide in the cache
        let slug = |r: &str| -> String {
            r.strip_prefix("docker://")
                .unwrap()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect()
        };
        let all = [
            slug("docker://postgres:16"),
            slug("docker://postgres:17"),
            slug("docker://ghcr.io/org/postgres:16"),
        ];
        let unique: std::collections::BTreeSet<_> = all.iter().collect();
        assert_eq!(unique.len(), all.len(), "cache keys collide: {all:?}");
    }
}

#[cfg(test)]
mod layer_tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    /// A tar with a read-only directory followed by a file inside it —
    /// exactly mysql:8's `etc/pki/ca-trust/.../directory-hash` at 0555.
    fn readonly_dir_then_child() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut dir = tar::Header::new_gnu();
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_path("ro-dir/").unwrap();
        dir.set_size(0);
        dir.set_mode(0o555); // no write bit: the whole problem
        dir.set_cksum();
        builder.append(&dir, std::io::empty()).unwrap();

        let body = b"cert";
        let mut file = tar::Header::new_gnu();
        file.set_entry_type(tar::EntryType::Regular);
        file.set_path("ro-dir/002c0b4f.0").unwrap();
        file.set_size(body.len() as u64);
        file.set_mode(0o444);
        file.set_cksum();
        builder.append(&file, &body[..]).unwrap();
        builder.into_inner().unwrap()
    }

    #[test]
    fn a_read_only_directory_does_not_block_its_own_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mut deferred = std::collections::BTreeMap::new();

        apply_layer_tar(
            &readonly_dir_then_child()[..],
            LayerComp::None,
            &rootfs,
            &mut deferred,
        )
        .expect("a 0555 directory must not fail the layer for non-root users");

        let child = rootfs.join("ro-dir/002c0b4f.0");
        assert!(
            child.exists(),
            "file inside the read-only dir was not written"
        );
        assert_eq!(std::fs::read(&child).unwrap(), b"cert");

        // still writable mid-extraction, so a later layer can add to it
        let mode = std::fs::metadata(rootfs.join("ro-dir"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o200, 0o200, "dir should stay writable until sealed");

        // and sealed to the image's real mode once every layer is applied
        restore_dir_modes(&deferred);
        let mode = std::fs::metadata(rootfs.join("ro-dir"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o555, "the image's mode must be restored");
    }

    #[test]
    fn deferred_modes_are_restored_deepest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("a");
        let inner = outer.join("b");
        std::fs::create_dir_all(&inner).unwrap();
        let mut deferred = std::collections::BTreeMap::new();
        deferred.insert(outer.clone(), 0o555);
        deferred.insert(inner.clone(), 0o555);

        // sealing the parent first would make the child unreachable to chmod
        restore_dir_modes(&deferred);
        for dir in [&inner, &outer] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o555, "{} not sealed", dir.display());
        }
        // leave the tree removable for tempdir cleanup
        for dir in [&outer, &inner] {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
        }
        let _ = std::io::stdout().flush();
    }
}

#[cfg(test)]
mod user_tests {
    use super::*;

    fn rootfs_with_accounts() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        std::fs::write(
            dir.path().join("etc/passwd"),
            "root:x:0:0:root:/root:/bin/sh\n\
             # a comment line that must not abort the search\n\
             memcache:x:11:11:memcache:/home/memcache:/sbin/nologin\n\
             postgres:x:70:70::/var/lib/postgresql:/bin/sh\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("etc/group"),
            "root:x:0:\nmemcache:x:11:\nstaff:x:50:\n",
        )
        .unwrap();
        dir
    }

    fn user(spec: Option<&str>) -> Option<String> {
        let dir = rootfs_with_accounts();
        let config = OciConfig {
            user: spec.map(str::to_string),
            ..Default::default()
        };
        user_from_config(&config, dir.path())
    }

    #[test]
    fn a_named_user_resolves_against_the_images_own_passwd() {
        // memcached exits 64 rather than run as root; this is what fixes it
        assert_eq!(user(Some("memcache")).as_deref(), Some("memcache:11:11"));
        assert_eq!(user(Some("postgres")).as_deref(), Some("postgres:70:70"));
    }

    #[test]
    fn every_oci_user_spelling_resolves() {
        assert_eq!(user(Some("11")).as_deref(), Some("memcache:11:11"));
        assert_eq!(user(Some("11:11")).as_deref(), Some("memcache:11:11"));
        assert_eq!(
            user(Some("memcache:staff")).as_deref(),
            Some("memcache:11:50")
        );
        assert_eq!(user(Some("memcache:50")).as_deref(), Some("memcache:11:50"));
    }

    #[test]
    fn absent_or_empty_user_stays_root() {
        assert_eq!(user(None), None);
        assert_eq!(user(Some("")), None);
        assert_eq!(user(Some("   ")), None);
    }

    #[test]
    fn an_unknown_name_is_dropped_rather_than_guessed() {
        // better to run as root, visibly, than to invent a uid and write
        // files nobody owns
        assert_eq!(user(Some("nosuchuser")), None);
    }

    #[test]
    fn a_numeric_id_the_image_does_not_know_still_works() {
        assert_eq!(user(Some("4242")).as_deref(), Some("uid4242:4242:4242"));
    }

    #[test]
    fn resolved_users_are_accepted_by_the_manifest_parser() {
        // the whole point: this string must survive [package] user parsing
        let spec = user(Some("memcache")).unwrap();
        let parsed = crate::manifest::parse_user(&spec).expect("round-trips");
        assert_eq!(parsed.name, "memcache");
        assert_eq!((parsed.uid, parsed.gid), (11, 11));
    }
}
