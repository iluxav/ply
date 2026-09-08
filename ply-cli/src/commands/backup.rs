//! `ply backup now|ls` and `ply restore` — the host-side verbs of a
//! service's self-backup contract.
//!
//! The contract is the service's, not ply's: a service that reads
//! `BACKUP_DEST` (an rclone target) dumps itself on `BACKUP_INTERVAL`,
//! keeps `BACKUP_KEEP_DAYS`, restores `BACKUP_RESTORE` into an empty
//! volume on first boot, and ships `backup.sh` (one dump, now) and
//! `restore.sh` (a dump from stdin, `--to NAME` or `--replace`) beside its
//! entrypoint. The registry's postgres does all of that. These verbs run
//! those scripts through `ply exec`, in the instance, with the instance's
//! own environment — so the destination and its credentials (sealed, if
//! you like) never have to be repeated on the command line.

use anyhow::{bail, Result};

use crate::cli::{BackupTarget, RestoreArgs};

/// Where the service's scripts live: its declared workdir, else the
/// package's own directory. Computed from the image, because the instance
/// may run under a stack alias (`db`) that is not the package's name.
fn service_dir(app: &str) -> Result<(String, String)> {
    let instance = ply_core::runtime::state::find(app)?;
    let manifest = ply_core::image::read::read_manifest(std::path::Path::new(&instance.image))?;
    let name = manifest.package.name.clone();
    let dir = manifest
        .package
        .workdir
        .clone()
        .unwrap_or_else(|| format!("/opt/{name}"));
    Ok((format!("{}.{}", instance.app, instance.n), dir))
}

/// The one-line preamble every verb runs inside the instance: the service
/// directory, and a refusal in plain words when the contract is not set up.
fn preamble(dir: &str) -> String {
    format!(
        r#"cd "{dir}" || {{ echo "ply: {dir} does not exist in the instance" >&2; exit 3; }}
[ -n "${{BACKUP_DEST:-}}" ] || {{ echo "ply: this service has no BACKUP_DEST — set it (and the destination's credentials) in the service's env; see https://plybox.sh/docs/backups/" >&2; exit 3; }}
[ -x ./backup.sh ] || {{ echo "ply: this service ships no backup.sh, so it does not take part in the backup contract" >&2; exit 3; }}
"#
    )
}

fn run(app: &str, script: String) -> Result<i32> {
    super::exec::run_in(app, &["/bin/sh".to_string(), "-c".to_string(), script])
}

pub fn now(args: &BackupTarget) -> Result<()> {
    let (instance, dir) = service_dir(&args.app)?;
    let code = run(&instance, format!("{}exec ./backup.sh", preamble(&dir)))?;
    if code != 0 {
        bail!("backup of {instance} failed (exit {code})");
    }
    Ok(())
}

pub fn ls(args: &BackupTarget) -> Result<()> {
    let (instance, dir) = service_dir(&args.app)?;
    let script = format!(
        r#"{}export RCLONE_CONFIG="${{RCLONE_CONFIG:-/dev/null}}"; rclone lsf "$BACKUP_DEST" | sort"#,
        preamble(&dir)
    );
    let code = run(&instance, script)?;
    if code != 0 {
        bail!("listing {instance}'s backups failed (exit {code})");
    }
    Ok(())
}

pub fn restore(args: &RestoreArgs) -> Result<()> {
    let mode = match (&args.to, args.replace) {
        (Some(db), false) => format!("--to '{}'", db.replace('\'', "")),
        // The live database's name comes from the instance's own env, in
        // the instance — never guessed on the host, never taken from PID 1
        // (which is ply's init, whose environment is ply's).
        (None, true) => r#"--replace "${POSTGRES_DB:?POSTGRES_DB is not set on this service, so there is no live database to replace — use --to NAME}""#.to_string(),
        _ => bail!(
            "say where: `--to NAME` restores beside the live data, `--replace` restores over it \
             (and loses what was written since the dump)"
        ),
    };
    let (instance, dir) = service_dir(&args.app)?;
    let name = args.name.replace('\'', "");
    let script = format!(
        r#"{}[ -x ./restore.sh ] || {{ echo "ply: this service ships no restore.sh" >&2; exit 3; }}
export RCLONE_CONFIG="${{RCLONE_CONFIG:-/dev/null}}"
name='{name}'
if [ "$name" = latest ]; then name=$(rclone lsf "$BACKUP_DEST" | sort | tail -1); fi
[ -n "$name" ] || {{ echo "ply: no backups at $BACKUP_DEST" >&2; exit 3; }}
echo "ply: restoring $name"
rclone cat "$BACKUP_DEST/$name" | ./restore.sh {mode}"#,
        preamble(&dir)
    );
    let code = run(&instance, script)?;
    if code != 0 {
        bail!("restore of {instance} failed (exit {code})");
    }
    Ok(())
}
