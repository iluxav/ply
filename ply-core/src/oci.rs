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
    for (i, layer) in doc.layers.iter().enumerate() {
        eprintln!(
            "ply: layer {}/{} {}",
            i + 1,
            doc.layers.len(),
            &layer.digest[..19.min(layer.digest.len())]
        );
        let response = client.get(&format!("blobs/{}", layer.digest), "*/*")?;
        let reader = response.into_body().into_reader();
        let gz = layer.media_type.ends_with("gzip") || layer.media_type.is_empty();
        apply_layer_tar(reader, gz, &rootfs)?;
    }

    // Synthesized ply manifest.
    let name = image_ref
        .repository
        .rsplit('/')
        .next()
        .unwrap_or("imported")
        .replace(['_', '.'], "-");
    let version = Version::parse(&image_ref.tag).unwrap_or_else(|_| Version::new(0, 0, 0));
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
    let manifest = Manifest {
        package: Package {
            name: name.clone(),
            version: version.clone(),
            entrypoint: Some(entrypoint),
            base: false,
            provides_abi: None,
            include: vec![],
            isolation: "ns".into(),
        },
        dependencies: Default::default(),
        env,
        ports,
        volumes: Default::default(),
        resources: None,
        requires: None,
        restart: None,
        layer: None,
        sources: Default::default(),
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
fn apply_layer_tar(reader: impl Read, gzipped: bool, rootfs: &Path) -> Result<()> {
    let reader: Box<dyn Read> = if gzipped {
        Box::new(flate2::read::GzDecoder::new(reader))
    } else {
        Box::new(reader)
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
        entry
            .unpack_in(rootfs)
            .map_err(|e| Error::Source(format!("unpack {}: {e}", path.display())))?;
    }
    Ok(())
}
