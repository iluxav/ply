//! Volume snapshots: an app's data, committed as a dated image, restored
//! by putting it back.
//!
//! # The mechanism, and why it needs nothing from the image
//!
//! A ply instance's state lives in its declared `[volumes]` — the rootfs is
//! read-only and the overlay's scratch is thrown away by design. So a
//! backup is a copy of the volumes, and ply can take that copy the way
//! `ply craft commit` takes an overlay: pack the directory as an image.
//! The one difference from craft is that a craft session has exited its
//! shell when it commits, and a running database has not. So the copy is
//! taken with the app's processes held still for the seconds it takes:
//! what comes out is exactly what a power cut would leave, which every
//! real database recovers from by replaying its log.
//!
//! The copy is streamed out of the instance through `ply exec`, as a tar,
//! by the app's own user — so it works identically rootful, rootless and
//! inside a microVM, needs no host path, and carries the in-container
//! ownership the restore must reproduce. Nobody who packages an image has
//! to do anything: a `[volumes]` entry is the whole contract.
//!
//! # Layout of a snapshot image
//!
//! `/volumes/<name>/…` per declared volume, keyed by NAME rather than by
//! the path inside the container, so a manifest that later moves a volume
//! still restores by name. `/.snapshot.toml` says which app, which slot,
//! when, and the name→path table it was taken with. `/.manifest.toml`
//! makes it an ordinary image: `ply inspect` reads it and `ply run` refuses
//! it (no entrypoint), which is right.
//!
//! # Restore
//!
//! A restore is a roll: `ply restore` writes a marker naming the slot and
//! the snapshot, then deploys the app's current image over itself. When
//! the run parent rolls that slot, `launch_instance` finds the marker,
//! moves the slot's volume directories aside, and hands the launch a
//! populate instruction; the container's init (Linux) or the guest's init
//! (macOS) fills the fresh volume from the image before the app starts —
//! inside the user namespace, so ownership comes out right rootless too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::image::name::{Arch, ImageName, Os};
use crate::image::read::{read_embedded, read_manifest, MANIFEST_PATH};
use crate::image::squashfs::{write_image_from_tar, ExtraFile, TarPlacement};
use crate::runtime::state::{self, InstanceState};

/// Inside a snapshot image: the per-volume trees.
pub const VOLUMES_PREFIX: &str = "/volumes";
/// Inside a snapshot image: what it is.
pub const META_PATH: &str = "/.snapshot.toml";
/// Beside the run parent's other per-app files: the pending restore.
pub const RESTORE_MARKER: &str = "restore-volumes";

/// What a snapshot is of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub app: String,
    pub slot: u32,
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`.
    pub taken: String,
    /// The app image the instance was running.
    pub image: String,
    /// Volume name → path inside the container, as declared when taken.
    pub volumes: BTreeMap<String, String>,
}

/// Where an app's snapshots live (the images themselves).
pub fn dir(app: &str) -> PathBuf {
    crate::paths::data_dir().join("snapshots").join(app)
}

/// Where a small JSON index of each snapshot lives, UNDER the apps dir —
/// so a reader with only the apps dir granted (the dashboard) can list an
/// app's snapshots without opening a squashfs. One `<name>.json` per image,
/// written by `take`, removed by `remove`.
pub fn index_dir(app: &str) -> PathBuf {
    crate::paths::apps_dir().join(app).join("snapshots")
}

/// The index line for one snapshot: its metadata plus what a listing shows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    pub bytes: u64,
    #[serde(flatten)]
    pub meta: Meta,
}

fn write_index(app: &str, name: &str, bytes: u64, meta: &Meta) {
    let dir = index_dir(app);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let index = Index {
        name: name.to_string(),
        bytes,
        meta: meta.clone(),
    };
    if let Ok(text) = serde_json::to_string(&index) {
        let tmp = dir.join(format!(".{name}.json.tmp"));
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join(format!("{name}.json")));
        }
    }
}

/// `YYYYMMDD.HHMMSS.<slot>`: a version that sorts by time and names the
/// slot, so the image's own name (`<app>-snapshot-<version>-linux-<arch>`)
/// says everything `ls` needs without opening it.
fn version_for(now: std::time::SystemTime, slot: u32) -> (semver::Version, String) {
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil(secs);
    let version = semver::Version::new(
        y as u64 * 10_000 + mo as u64 * 100 + d as u64,
        h as u64 * 10_000 + mi as u64 * 100 + s as u64,
        slot as u64,
    );
    let taken = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    (version, taken)
}

/// Days-since-epoch → civil date, the standard algorithm; no crate needed.
fn civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// The shell that runs inside the instance: hold every process of the
/// app's user still, stream the volumes as one tar, let them go. The thaw
/// runs BOTH on the normal path and from a trap, and the tar is NOT
/// `exec`'d — an `exec` would replace the shell and discard the trap, so a
/// frozen app would never be woken (found exactly this way: postgres left
/// stopped, then gone, after a snapshot). `kill -STOP -1` reaches every
/// process this user may signal except the caller; PID 1 of the namespace
/// is immune to it by kernel rule, which is exactly right.
///
/// `tar` is Essential in Debian and present in every busybox, which covers
/// the registry and Docker imports; an image built from a static binary
/// alone has no tar and no volumes worth the name.
fn snapshot_script() -> &'static str {
    r#"thaw() { kill -CONT -1 2>/dev/null; }
trap thaw EXIT INT TERM
for p in "$@"; do [ -d "$p" ] || { echo "ply snapshot: $p is not a directory in the instance" >&2; exit 3; }; done
command -v tar >/dev/null 2>&1 || { echo "ply snapshot: the image has no tar" >&2; exit 3; }
kill -STOP -1 2>/dev/null
sync 2>/dev/null
rel=""; for p in "$@"; do rel="$rel ${p#/}"; done
cd / && tar -cf - $rel
status=$?
thaw
exit $status"#
}

/// What `take` produced for one instance.
#[derive(Debug, Clone)]
pub struct Taken {
    pub instance: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub volumes: Vec<String>,
}

/// The instances `target` names: `app.n` exactly, or every live instance
/// of `app`.
fn instances_of(target: &str) -> Result<Vec<InstanceState>> {
    let exact = target
        .rsplit_once('.')
        .and_then(|(_, n)| n.parse::<u32>().ok())
        .is_some();
    if exact {
        return Ok(vec![state::find(target)?]);
    }
    let all: Vec<InstanceState> = state::list()?
        .into_iter()
        .filter(|s| s.app == target && s.alive())
        .collect();
    if all.is_empty() {
        return Err(Error::Runtime(format!(
            "no running instance of `{target}` — a snapshot is taken from a running app"
        )));
    }
    Ok(all)
}

/// Take a snapshot of every declared volume of each instance `target`
/// names. `ply` is the binary to run `exec` through (the caller's own).
pub fn take(target: &str, ply: &Path) -> Result<Vec<Taken>> {
    let mut out = Vec::new();
    for instance in instances_of(target)? {
        out.push(take_one(&instance, ply)?);
    }
    Ok(out)
}

fn take_one(instance: &InstanceState, ply: &Path) -> Result<Taken> {
    let manifest = read_manifest(Path::new(&instance.image))?;
    let volumes: BTreeMap<String, String> = manifest
        .volumes
        .iter()
        .map(|(name, v)| (name.clone(), v.path.clone()))
        .collect();
    if volumes.is_empty() {
        return Err(Error::Runtime(format!(
            "{} declares no [volumes] — there is no data to snapshot (its rootfs is read-only and \
             its scratch is thrown away by design)",
            manifest.package.name
        )));
    }
    let name = format!("{}.{}", instance.app, instance.n);
    let dir = dir(&instance.app);
    std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
        path: dir.clone(),
        source,
    })?;
    let (version, taken) = version_for(std::time::SystemTime::now(), instance.n);
    let image_name = ImageName::new(
        &format!("{}-snapshot", instance.app),
        version,
        Os::Linux,
        Arch::host(),
    )?;
    let final_path = dir.join(image_name.to_string());
    let spool = dir.join(format!(".{}.tar", image_name));
    let tmp_img = dir.join(format!(".{}.tmp", image_name));

    // Stream the tar out of the instance into the spool.
    let mut args: Vec<String> = vec![
        "exec".into(),
        name.clone(),
        "/bin/sh".into(),
        "-c".into(),
        snapshot_script().into(),
        "sh".into(),
    ];
    args.extend(volumes.values().cloned());
    let spool_file = std::fs::File::create(&spool).map_err(|source| Error::Io {
        path: spool.clone(),
        source,
    })?;
    let status = std::process::Command::new(ply)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(spool_file)
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|source| Error::Io {
            path: ply.to_path_buf(),
            source,
        })?;
    if !status.success() {
        let _ = std::fs::remove_file(&spool);
        return Err(Error::Runtime(format!(
            "snapshot of {name} failed: the copy inside the instance exited {}",
            status.code().unwrap_or(-1)
        )));
    }

    // Place each tar entry under /volumes/<name>/… by the volume whose
    // container path it fell under.
    let by_path: Vec<(String, String)> = volumes
        .iter()
        .map(|(n, p)| (p.trim_matches('/').to_string(), n.clone()))
        .collect();
    let place = move |entry: &Path| -> Option<TarPlacement> {
        let entry = entry.to_string_lossy();
        let entry = entry.trim_start_matches("./").trim_end_matches('/');
        for (vol_path, vol_name) in &by_path {
            if entry == vol_path {
                return Some(TarPlacement {
                    dest: format!("{VOLUMES_PREFIX}/{vol_name}"),
                });
            }
            if let Some(rest) = entry.strip_prefix(&format!("{vol_path}/")) {
                return Some(TarPlacement {
                    dest: format!("{VOLUMES_PREFIX}/{vol_name}/{rest}"),
                });
            }
        }
        None
    };
    let meta = Meta {
        app: instance.app.clone(),
        slot: instance.n,
        taken,
        image: instance.image.clone(),
        volumes: volumes.clone(),
    };
    let manifest_text = format!(
        "[package]\nname = \"{}-snapshot\"\nversion = \"{}\"\ndescription = \"volume snapshot of {} taken {}\"\n",
        instance.app, image_name.version, name, meta.taken
    );
    let extra = [
        ExtraFile {
            path: MANIFEST_PATH.into(),
            bytes: manifest_text.into_bytes(),
            mode: 0o444,
        },
        ExtraFile {
            path: META_PATH.into(),
            bytes: toml::to_string_pretty(&meta)
                .map_err(|e| Error::Build(e.to_string()))?
                .into_bytes(),
            mode: 0o444,
        },
    ];
    let written = write_image_from_tar(&spool, &place, &extra, &tmp_img);
    let _ = std::fs::remove_file(&spool);
    written?;
    std::fs::rename(&tmp_img, &final_path).map_err(|source| Error::Io {
        path: final_path.clone(),
        source,
    })?;
    let bytes = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
    write_index(&instance.app, &image_name.to_string(), bytes, &meta);
    Ok(Taken {
        instance: name,
        path: final_path,
        bytes,
        volumes: volumes.keys().cloned().collect(),
    })
}

/// One snapshot on disk.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub meta: Meta,
}

/// Read a snapshot's metadata.
pub fn meta_of(path: &Path) -> Result<Meta> {
    let bytes = read_embedded(path, META_PATH)?
        .ok_or_else(|| Error::Runtime(format!("{}: not a snapshot image", path.display())))?;
    toml::from_str(&String::from_utf8_lossy(&bytes))
        .map_err(|e| Error::Runtime(format!("{}: bad snapshot metadata: {e}", path.display())))
}

/// Every snapshot of `app`, oldest first.
pub fn list(app: &str) -> Result<Vec<Entry>> {
    let dir = dir(app);
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(source) => return Err(Error::Io { path: dir, source }),
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let file = entry.file_name().to_string_lossy().into_owned();
        if !file.ends_with(".img") || file.starts_with('.') {
            continue;
        }
        let Ok(meta) = meta_of(&path) else { continue };
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        out.push(Entry {
            name: file.trim_end_matches(".img").to_string(),
            path,
            bytes,
            meta,
        });
    }
    out.sort_by(|a, b| a.meta.taken.cmp(&b.meta.taken).then(a.name.cmp(&b.name)));
    Ok(out)
}

/// The snapshot `which` names: a snapshot's name as `ls` shows it (with or
/// without `.img`), or `latest`, optionally for one slot (`latest` picks
/// the newest of any slot).
pub fn resolve(app: &str, which: &str) -> Result<Entry> {
    let all = list(app)?;
    if all.is_empty() {
        return Err(Error::Runtime(format!(
            "no snapshots of `{app}` — `ply snapshot take {app}` makes one"
        )));
    }
    if which == "latest" {
        return Ok(all.last().cloned().expect("non-empty"));
    }
    let wanted = which.trim_end_matches(".img");
    all.into_iter().find(|e| e.name == wanted).ok_or_else(|| {
        Error::Runtime(format!(
            "no snapshot `{which}` of `{app}` — `ply snapshot ls {app}` lists them"
        ))
    })
}

pub fn remove(app: &str, which: &str) -> Result<Entry> {
    let entry = resolve(app, which)?;
    std::fs::remove_file(&entry.path).map_err(|source| Error::Io {
        path: entry.path.clone(),
        source,
    })?;
    let _ = std::fs::remove_file(index_dir(app).join(format!("{}.json", entry.name)));
    Ok(entry)
}

/// Write the restore marker for `entry`'s slot, for the run parent to
/// consume on its next roll of that slot. This is the half of `restore`
/// that does not itself roll: the CLI follows it with a `deploy`, and the
/// run parent (handling a `restore` control command) follows it by queuing
/// the slot in its own roll.
pub fn mark_restore(app: &str, entry: &Entry) -> Result<()> {
    let marker = marker_path(app);
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(
        &marker,
        format!("{} {}\n", entry.meta.slot, entry.path.display()),
    )
    .map_err(|source| Error::Io {
        path: marker,
        source,
    })
}

/// One line of the restore marker: which slot, from which image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub slot: u32,
    pub image: PathBuf,
}

pub fn marker_path(app: &str) -> PathBuf {
    crate::paths::apps_dir().join(app).join(RESTORE_MARKER)
}

/// Ask the run parent to restore `entry` into its slot on the next roll,
/// then roll. Returns the deploy report the roll produced.
pub fn restore(
    app: &str,
    entry: &Entry,
    timeout_secs: u64,
) -> Result<crate::lifecycle::DeployReport> {
    let instance = state::list()?
        .into_iter()
        .find(|s| s.app == app && s.n == entry.meta.slot && s.alive())
        .ok_or_else(|| {
            Error::Runtime(format!(
                "{app}.{} is not running — a snapshot restores into the slot it was taken from, \
                 and the app must be up for the roll (start it, then restore)",
                entry.meta.slot
            ))
        })?;
    mark_restore(app, entry)?;
    // The roll: the app's current image over itself. The parent's launch
    // for the marked slot does the actual restore.
    let image = instance
        .launch_path
        .clone()
        .unwrap_or_else(|| instance.image.clone());
    let report = crate::lifecycle::deploy(Path::new(&image), timeout_secs);
    // A marker nobody consumed (the roll never reached the slot) must not
    // ambush the next ordinary deploy.
    let _ = std::fs::remove_file(marker_path(app));
    report
}

/// The marker's lines for `slot`, consumed. Called by the run parent at
/// launch; nothing else reads the marker.
pub fn take_pending(app: &str, slot: u32) -> Vec<Pending> {
    let marker = marker_path(app);
    let Ok(text) = std::fs::read_to_string(&marker) else {
        return Vec::new();
    };
    let mut mine = Vec::new();
    let mut rest = String::new();
    for line in text.lines() {
        match parse_marker_line(line) {
            Some(p) if p.slot == slot => mine.push(p),
            Some(_) => {
                rest.push_str(line);
                rest.push('\n');
            }
            None => {}
        }
    }
    if rest.is_empty() {
        let _ = std::fs::remove_file(&marker);
    } else {
        let _ = std::fs::write(&marker, rest);
    }
    mine
}

fn parse_marker_line(line: &str) -> Option<Pending> {
    let (slot, image) = line.trim().split_once(' ')?;
    Some(Pending {
        slot: slot.parse().ok()?,
        image: PathBuf::from(image.trim()),
    })
}

/// Move a volume directory aside before a restore fills a fresh one. Kept,
/// not deleted: a restore that goes wrong must never have destroyed the
/// data it replaced. Under a dot-directory so it is not mistaken for a
/// volume slot.
pub fn set_aside(host_dir: &Path) -> Result<Option<PathBuf>> {
    if !host_dir.exists() {
        return Ok(None);
    }
    let parent = host_dir.parent().unwrap_or(Path::new("."));
    let trash = parent.join(".pre-restore");
    std::fs::create_dir_all(&trash).map_err(|source| Error::Io {
        path: trash.clone(),
        source,
    })?;
    let (_, when) = version_for(std::time::SystemTime::now(), 0);
    let name = host_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "volume".into());
    let aside = trash.join(format!("{name}-{}", when.replace([':', '-'], "")));
    std::fs::rename(host_dir, &aside).map_err(|source| Error::Io {
        path: host_dir.to_path_buf(),
        source,
    })?;
    Ok(Some(aside))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_version_sorts_by_time_and_names_the_slot() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_788_870_335);
        let (v, taken) = version_for(t, 2);
        assert_eq!(taken, "2026-09-08T12:25:35Z");
        assert_eq!(v.to_string(), "20260908.122535.2");
        let (later, _) = version_for(t + std::time::Duration::from_secs(1), 1);
        assert!(later > v);
    }

    #[test]
    fn civil_dates_are_right_at_the_edges() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(civil(1_704_067_199), (2023, 12, 31, 23, 59, 59));
    }

    #[test]
    fn a_tar_of_two_volumes_becomes_one_image_with_ownership_kept() {
        // Build a tar the way the instance would: paths relative to /,
        // owned by a non-root uid.
        let dir = tempfile::tempdir().unwrap();
        let tar_path = dir.path().join("spool.tar");
        {
            let mut b = tar::Builder::new(std::fs::File::create(&tar_path).unwrap());
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_mode(0o700);
            h.set_uid(70);
            h.set_gid(70);
            h.set_size(0);
            h.set_cksum();
            b.append_data(&mut h, "var/lib/postgresql/data", std::io::empty())
                .unwrap();
            let data = b"hello";
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_mode(0o600);
            h.set_uid(70);
            h.set_gid(70);
            h.set_size(data.len() as u64);
            h.set_cksum();
            b.append_data(&mut h, "var/lib/postgresql/data/PG_VERSION", &data[..])
                .unwrap();
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_mode(0o644);
            h.set_uid(1000);
            h.set_gid(1000);
            h.set_size(3);
            h.set_cksum();
            b.append_data(&mut h, "srv/uploads/a.txt", &b"abc"[..])
                .unwrap();
            b.finish().unwrap();
        }
        let volumes: BTreeMap<String, String> = [
            ("data".to_string(), "/var/lib/postgresql/data".to_string()),
            ("uploads".to_string(), "/srv/uploads".to_string()),
        ]
        .into();
        let by_path: Vec<(String, String)> = volumes
            .iter()
            .map(|(n, p)| (p.trim_matches('/').to_string(), n.clone()))
            .collect();
        let place = move |entry: &Path| -> Option<TarPlacement> {
            let e = entry.to_string_lossy();
            let e = e.trim_start_matches("./").trim_end_matches('/');
            for (vp, vn) in &by_path {
                if e == vp {
                    return Some(TarPlacement {
                        dest: format!("{VOLUMES_PREFIX}/{vn}"),
                    });
                }
                if let Some(rest) = e.strip_prefix(&format!("{vp}/")) {
                    return Some(TarPlacement {
                        dest: format!("{VOLUMES_PREFIX}/{vn}/{rest}"),
                    });
                }
            }
            None
        };
        let out = dir.path().join("snap.img");
        write_image_from_tar(&tar_path, &place, &[], &out).unwrap();

        // Extract with ownership as the restore would (as this user, so
        // only our own uid can be applied; the headers are still checked).
        let file = std::fs::File::open(&out).unwrap();
        let fs = backhand::FilesystemReader::from_reader(std::io::BufReader::new(file)).unwrap();
        let mut seen: BTreeMap<String, (u32, u32, u16)> = BTreeMap::new();
        for node in fs.files() {
            seen.insert(
                node.fullpath.to_string_lossy().into_owned(),
                (node.header.uid, node.header.gid, node.header.permissions),
            );
        }
        assert_eq!(seen["/volumes/data/PG_VERSION"], (70, 70, 0o600));
        assert_eq!(seen["/volumes/data"], (70, 70, 0o700));
        assert_eq!(seen["/volumes/uploads/a.txt"], (1000, 1000, 0o644));
        // The tree under a volume comes back byte for byte.
        let dest = dir.path().join("restored");
        std::fs::create_dir(&dest).unwrap();
        // The mount point starts loose, as a launch leaves it.
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::image::extract::extract_subtree(&out, "/volumes/data", &dest, false).unwrap();
        assert_eq!(std::fs::read(dest.join("PG_VERSION")).unwrap(), b"hello");
        assert!(!dest.join("a.txt").exists(), "the other volume stays out");
        // …and the mount point itself took the snapshot's 0700 — postgres
        // refuses to start from anything looser.
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o700,
            "the data dir's own mode is restored, not just its contents"
        );
    }

    #[test]
    fn the_marker_hands_each_slot_its_own_lines_and_keeps_the_rest() {
        assert_eq!(
            parse_marker_line("2 /var/lib/ply/snapshots/db/x.img"),
            Some(Pending {
                slot: 2,
                image: PathBuf::from("/var/lib/ply/snapshots/db/x.img")
            })
        );
        assert_eq!(parse_marker_line("nonsense"), None);
    }
}
