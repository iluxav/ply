//! `ply craft` — interactive package authoring.
//!
//! The overlay upperdir IS the layer: run a persistent session on a resolved
//! base closure, install things with real root, then pack `rw/` as an inert,
//! content-addressed package. Imperative authoring, declarative artifact.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::image::name::{Arch, ImageName, Os};
use crate::image::read::MANIFEST_PATH;
use crate::image::squashfs::{write_image, ExtraFile, TreeSource};
use crate::lockfile::{LockedPackage, Lockfile};
use crate::manifest::{Base, Manifest, Package};
use crate::resolve::Resolver;
use crate::runtime::container::{child_main, ContainerSpec};
use crate::runtime::{loopdev, mount};
use crate::source::Source;
use crate::store::Store;

#[derive(Debug, Serialize, Deserialize)]
pub struct CraftSession {
    pub name: String,
    /// The `--from` spec as given: package, constraint, source.
    pub from_package: String,
    pub from_constraint: String,
    pub from_source: String,
    /// Resolved closure (overlay order) captured at `craft new`.
    pub lockfile: Lockfile,
    pub created: u64,
}

fn session_dir(name: &str) -> PathBuf {
    crate::paths::craft_dir().join(name)
}

impl CraftSession {
    fn save(&self) -> Result<()> {
        let path = session_dir(&self.name).join("session.json");
        std::fs::write(&path, serde_json::to_vec_pretty(self).expect("serializes"))
            .map_err(|source| Error::Io { path, source })
    }

    pub fn load(name: &str) -> Result<CraftSession> {
        let path = session_dir(name).join("session.json");
        let text = std::fs::read_to_string(&path).map_err(|_| {
            Error::Runtime(format!(
                "no craft session `{name}` — create one with `ply craft new {name} --from pkg@version`"
            ))
        })?;
        serde_json::from_str(&text)
            .map_err(|e| Error::Runtime(format!("{}: corrupt session: {e}", path.display())))
    }
}

/// `ply craft new` — resolve the base closure, create the session, open a shell.
pub fn new(
    name: &str,
    from: &str,
    source: Option<&str>,
    cmd: &[String],
    allow_insecure: bool,
) -> Result<i32> {
    crate::manifest::validate_package_name(name)?;
    let dir = session_dir(name);
    if dir.exists() {
        return Err(Error::Runtime(format!(
            "craft session `{name}` already exists — `ply craft shell {name}` to continue it, `ply craft rm {name}` to discard"
        )));
    }

    let (package, constraint) = from.split_once('@').ok_or_else(|| {
        Error::Runtime(format!(
            "--from `{from}`: expected pkg@constraint, e.g. alpine@3.20"
        ))
    })?;
    let source_spec = source
        .ok_or_else(|| {
            Error::Runtime("--source URL is required (where to fetch the base from)".into())
        })?
        .to_string();

    // Resolve exactly like an app with one dependency would.
    let synthetic = Manifest::parse(&format!(
        "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\n\n[dependencies]\n{package} = \"{constraint}\"\n\n[sources]\ndefault = \"{source_spec}\"\n"
    ))?;
    let store = Store::open_default()?;
    let mut resolver = Resolver::new(&synthetic, &store, Arch::host(), allow_insecure);
    let resolution = resolver.resolve()?;
    let lockfile = Lockfile {
        packages: resolution
            .packages
            .iter()
            .map(|p| LockedPackage {
                name: p.name.clone(),
                version: p.version.clone(),
                source: p.source_spec.clone(),
                sha256: p.digest.clone(),
            })
            .collect(),
    };

    for sub in ["rw", "work", "root", "layers"] {
        std::fs::create_dir_all(dir.join(sub)).map_err(|source| Error::Io {
            path: dir.join(sub),
            source,
        })?;
    }
    let session = CraftSession {
        name: name.to_string(),
        from_package: package.to_string(),
        from_constraint: constraint.to_string(),
        from_source: source_spec,
        lockfile,
        created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    session.save()?;
    shell(name, cmd)
}

/// `ply craft edit` — reconstruct a session from a committed package image:
/// the image IS the upperdir at commit time, so resuming is lossless on any
/// machine that has the file.
pub fn edit(image: &Path, source_override: Option<&str>, allow_insecure: bool) -> Result<String> {
    if !nix::unistd::geteuid().is_root() {
        return Err(Error::Runtime("ply craft needs root — try sudo".into()));
    }
    let manifest = crate::image::read::read_manifest(image)?;
    let name = manifest.package.name.clone();
    let dir = session_dir(&name);
    if dir.exists() {
        return Err(Error::Runtime(format!(
            "craft session `{name}` already exists locally — `ply craft rm {name}` first (or continue it with `ply craft shell {name}`)"
        )));
    }
    let Some(spec) = manifest.base_dep() else {
        return Err(Error::Runtime(format!(
            "{}: craft edit needs a package that records its base (`[package] base = …`) — this one doesn't (rebuild or re-commit it with a current ply)",
            image.display()
        )));
    };
    let source_spec = source_override
        .map(str::to_string)
        .or(spec.source.clone())
        .ok_or_else(|| {
            Error::Runtime("the image's dependency has no source — pass --source URL".into())
        })?;

    // Resolve the base closure exactly like `craft new`.
    let synthetic = Manifest::parse(&format!(
        "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\n\n[dependencies]\n{} = \"{}\"\n\n[sources]\ndefault = \"{source_spec}\"\n",
        spec.package, spec.constraint
    ))?;
    let store = Store::open_default()?;
    let mut resolver = Resolver::new(&synthetic, &store, Arch::host(), allow_insecure);
    let resolution = resolver.resolve()?;
    let lockfile = Lockfile {
        packages: resolution
            .packages
            .iter()
            .map(|p| LockedPackage {
                name: p.name.clone(),
                version: p.version.clone(),
                source: p.source_spec.clone(),
                sha256: p.digest.clone(),
            })
            .collect(),
    };

    for sub in ["rw", "work", "root", "layers"] {
        std::fs::create_dir_all(dir.join(sub)).map_err(|source| Error::Io {
            path: dir.join(sub),
            source,
        })?;
    }
    // The image's tree becomes the upperdir again.
    crate::image::extract::extract_rootfs(image, &dir.join("rw"))?;
    for meta in [".manifest.toml", ".lock.toml", ".layer.toml"] {
        let _ = std::fs::remove_file(dir.join("rw").join(meta));
    }

    let session = CraftSession {
        name: name.clone(),
        from_package: spec.package.clone(),
        from_constraint: spec.constraint.clone(),
        from_source: source_spec,
        lockfile,
        created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    session.save()?;
    Ok(name)
}

/// `ply craft shell` — (re)enter the session.
pub fn shell(name: &str, cmd: &[String]) -> Result<i32> {
    if !nix::unistd::geteuid().is_root() {
        return Err(Error::Runtime("ply craft needs root — try sudo".into()));
    }
    let session = CraftSession::load(name)?;
    let dir = session_dir(name);
    let store = Store::open_default()?;

    // Mount the closure (fetch anything missing by digest).
    let mut mounted: Vec<PathBuf> = Vec::new();
    let guard_targets = std::cell::RefCell::new(Vec::new());
    let _unmount_guard = scopeguard(&guard_targets);
    for (i, pkg) in session.lockfile.packages.iter().enumerate() {
        let img = match store.image_path(&pkg.sha256) {
            Some(path) => path,
            None => {
                let source = Source::parse(&pkg.source, true)?;
                let image =
                    ImageName::new(&pkg.name, pkg.version.clone(), Os::Linux, Arch::host())?;
                eprintln!("ply: fetching {image} ({})", pkg.sha256);
                source.fetch(&image, Some(&pkg.sha256), &store)?.1
            }
        };
        let target = dir.join("layers").join(i.to_string());
        let (device, dev_fd) = loopdev::attach_ro(&img)?;
        mount::mount_squashfs_ro(&device, &target)?;
        drop(dev_fd);
        guard_targets.borrow_mut().push(target.clone());
        mounted.push(target);
    }

    let argv: Vec<String> = if cmd.is_empty() {
        vec!["/bin/sh".into()]
    } else {
        cmd.to_vec()
    };
    let (sync_rx, sync_tx) =
        nix::unistd::pipe().map_err(|e| Error::Runtime(format!("pipe: {e}")))?;
    let spec = ContainerSpec {
        layers: mounted,
        instance_dir: dir.clone(),
        hostname: format!("craft-{name}"),
        cwd: PathBuf::from("/"),
        env: vec![
            (
                "PATH".into(),
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
            ),
            ("HOME".into(), "/root".into()),
            (
                "TERM".into(),
                std::env::var("TERM").unwrap_or_else(|_| "xterm".into()),
            ),
            ("PS1".into(), format!("craft-{name}:\\w # ")),
        ],
        argv,
        binds: vec![],
        sync_rx,
        keep_caps: vec![],
        privileged: true, // authoring needs real root; host netns for apk/apt
        rootless: false,
        run_user: None,
    };

    // Host network (no CLONE_NEWNET): package managers need the internet,
    // and ply has no NAT. Craft sessions are trusted by definition.
    let mut stack = vec![0u8; 1024 * 1024];
    let flags = nix::sched::CloneFlags::CLONE_NEWNS
        | nix::sched::CloneFlags::CLONE_NEWPID
        | nix::sched::CloneFlags::CLONE_NEWUTS
        | nix::sched::CloneFlags::CLONE_NEWIPC;
    let child = unsafe {
        nix::sched::clone(
            Box::new(|| child_main(&spec)),
            &mut stack,
            flags,
            Some(nix::libc::SIGCHLD),
        )
    }
    .map_err(|e| Error::Runtime(format!("clone: {e}")))?;
    let _ = nix::unistd::write(&sync_tx, &[1u8]);
    drop(sync_tx);

    let code = loop {
        match nix::sys::wait::waitpid(child, None) {
            Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => break code,
            Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => break 128 + sig as i32,
            Ok(_) => continue,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(Error::Runtime(format!("waitpid: {e}"))),
        }
    };
    eprintln!(
        "ply: session `{name}` kept — `ply craft changes {name}` to inspect, `ply craft commit {name}` to package"
    );
    Ok(code)
}

struct UnmountGuard<'a>(&'a std::cell::RefCell<Vec<PathBuf>>);
impl Drop for UnmountGuard<'_> {
    fn drop(&mut self) {
        for target in self.0.borrow().iter() {
            mount::unmount_detach(target);
        }
    }
}
fn scopeguard(targets: &std::cell::RefCell<Vec<PathBuf>>) -> UnmountGuard<'_> {
    UnmountGuard(targets)
}

#[derive(Debug, PartialEq)]
pub enum Change {
    Added(PathBuf),
    Modified(PathBuf),
    Deleted(PathBuf),
}

/// `ply craft changes` — classify the upperdir against the layer closure.
pub fn changes(name: &str) -> Result<Vec<Change>> {
    let session = CraftSession::load(name)?;
    let store = Store::open_default()?;

    // Paths present in any lower layer → distinguishes M from A.
    let mut lower_paths: BTreeSet<PathBuf> = BTreeSet::new();
    for pkg in &session.lockfile.packages {
        if let Some(img) = store.image_path(&pkg.sha256) {
            let file = std::fs::File::open(&img).map_err(|source| Error::Io {
                path: img.clone(),
                source,
            })?;
            if let Ok(fs) = backhand::FilesystemReader::from_reader(std::io::BufReader::new(file)) {
                for node in fs.files() {
                    lower_paths.insert(node.fullpath.clone());
                }
            }
        }
    }

    let rw = session_dir(name).join("rw");
    let mut result = Vec::new();
    for entry in walkdir::WalkDir::new(&rw).min_depth(1).sort_by_file_name() {
        let entry = entry.map_err(|e| Error::Runtime(format!("walking rw: {e}")))?;
        let rel = PathBuf::from("/").join(entry.path().strip_prefix(&rw).unwrap());
        let meta = entry
            .path()
            .symlink_metadata()
            .map_err(|source| Error::Io {
                path: entry.path().to_path_buf(),
                source,
            })?;
        if is_whiteout(&meta) {
            result.push(Change::Deleted(rel));
        } else if meta.is_dir() {
            continue; // dirs are structure, not content
        } else if lower_paths.contains(&rel) {
            result.push(Change::Modified(rel));
        } else {
            result.push(Change::Added(rel));
        }
    }
    Ok(result)
}

fn is_whiteout(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    meta.file_type().is_char_device() && meta.rdev() == 0
}

pub struct CommitOutcome {
    pub image_path: PathBuf,
    pub image_name: ImageName,
    pub digest: String,
    pub size_bytes: u64,
    pub skipped_deletions: usize,
}

/// `ply craft commit` — pack the upperdir as a package image.
pub fn commit(name: &str, version: &Version, output: Option<&Path>) -> Result<CommitOutcome> {
    let session = CraftSession::load(name)?;
    let rw = session_dir(name).join("rw");

    // Package manifest: the session's --from becomes `[package] base`, so
    // `craft edit` can resume from the artifact alone.
    let manifest = Manifest {
        package: Package {
            name: name.to_string(),
            version: version.clone(),
            entrypoint: None,
            base: Base::Detailed {
                name: session.from_package.clone(),
                version: session.from_constraint.clone(),
                source: Some(session.from_source.clone()),
            },
            provides_abi: None,
            user: None,
            workdir: None,
            capabilities: None,
            stop_signal: None,
            include: vec![],
            isolation: "ns".into(),
        },
        dependencies: Default::default(),
        env: Default::default(),
        ports: Default::default(),
        volumes: Default::default(),
        resources: None,
        requires: None,
        health: None,
        restart: None,
        layer: None,
        sources: Default::default(),
    };

    // Deletions can't ship yet (the writer refuses device nodes) — count and warn.
    let mut skipped_deletions = 0;
    for change in changes(name)? {
        if matches!(change, Change::Deleted(_)) {
            skipped_deletions += 1;
        }
    }
    let rw_for_filter = rw.clone();
    let filter = move |rel: &Path| -> bool {
        rw_for_filter
            .join(rel)
            .symlink_metadata()
            .map(|m| !is_whiteout(&m))
            .unwrap_or(true)
    };

    let image_name = ImageName::new(name, version.clone(), Os::Linux, Arch::host())?;
    let image_path = match output {
        Some(path) => path.to_path_buf(),
        None => PathBuf::from(image_name.to_string()),
    };
    let trees = [TreeSource {
        dir: &rw,
        prefix: "", // an addon overlays real paths, not a keg
        filter: Some(&filter),
    }];
    let extra = [ExtraFile {
        path: MANIFEST_PATH.into(),
        bytes: manifest.to_toml()?.into_bytes(),
        mode: 0o444,
    }];
    let tmp_out = image_path.with_extension("img.tmp");
    write_image(&trees, &extra, &tmp_out)?;
    std::fs::rename(&tmp_out, &image_path).map_err(|source| Error::Io {
        path: image_path.clone(),
        source,
    })?;

    // Under sudo, hand the artifact back to the invoking user.
    if let (Ok(uid), Ok(gid)) = (std::env::var("SUDO_UID"), std::env::var("SUDO_GID")) {
        if let (Ok(uid), Ok(gid)) = (uid.parse::<u32>(), gid.parse::<u32>()) {
            let _ = std::os::unix::fs::chown(&image_path, Some(uid), Some(gid));
        }
    }

    Ok(CommitOutcome {
        digest: crate::digest::sha256_file(&image_path)?,
        size_bytes: std::fs::metadata(&image_path)
            .map_err(|source| Error::Io {
                path: image_path.clone(),
                source,
            })?
            .len(),
        image_path,
        image_name,
        skipped_deletions,
    })
}

pub fn list() -> Result<Vec<String>> {
    let mut names = Vec::new();
    let dir = crate::paths::craft_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(names),
        Err(source) => return Err(Error::Io { path: dir, source }),
    };
    for entry in entries.filter_map(|e| e.ok()) {
        if entry.path().join("session.json").exists() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

pub fn rm(name: &str) -> Result<bool> {
    let dir = session_dir(name);
    if !dir.exists() {
        return Ok(false);
    }
    // Defensive: unmount any leftover layer mounts before deleting.
    if let Ok(entries) = std::fs::read_dir(dir.join("layers")) {
        for entry in entries.filter_map(|e| e.ok()) {
            mount::unmount_detach(&entry.path());
        }
    }
    crate::paths::force_remove_dir_all(&dir).map_err(|source| Error::Io { path: dir, source })?;
    Ok(true)
}
