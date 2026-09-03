//! Tier 2 + 3: rights stripping and seccomp.
//!
//! Secure by default, no knobs. Applied in the child, after all privileged
//! setup and immediately before exec.

use std::collections::BTreeMap;

use caps::{CapSet, Capability};
use nix::mount::{mount, MsFlags};

use crate::error::{Error, Result};

/// Paths in /proc that leak host state — masked with /dev/null or made ro.
const MASKED_PROC: &[&str] = &[
    "/proc/kcore",
    "/proc/keys",
    "/proc/timer_list",
    "/proc/sched_debug",
    "/proc/latency_stats",
];
const READONLY_PROC: &[&str] = &["/proc/sys", "/proc/sysrq-trigger", "/proc/irq", "/proc/bus"];

/// Bind /dev/null over sensitive proc files; remount config trees read-only.
/// Call after /proc and /dev are mounted.
pub fn mask_proc() -> Result<()> {
    for path in MASKED_PROC {
        if std::path::Path::new(path).exists() {
            mount(
                Some("/dev/null"),
                *path,
                None::<&str>,
                MsFlags::MS_BIND,
                None::<&str>,
            )
            .map_err(|e| Error::Runtime(format!("mask {path}: {e}")))?;
        }
    }
    for path in READONLY_PROC {
        if !std::path::Path::new(path).exists() {
            continue;
        }
        mount(
            Some(*path),
            *path,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .map_err(|e| Error::Runtime(format!("bind {path}: {e}")))?;
        mount(
            Some(*path),
            *path,
            None::<&str>,
            MsFlags::MS_BIND
                | MsFlags::MS_REMOUNT
                | MsFlags::MS_RDONLY
                | MsFlags::MS_NOSUID
                | MsFlags::MS_NODEV
                | MsFlags::MS_NOEXEC,
            None::<&str>,
        )
        .map_err(|e| Error::Runtime(format!("remount ro {path}: {e}")))?;
    }
    Ok(())
}

/// Docker's default capability set, which every official image is built
/// against — its entrypoint expects to `chown` the data dir and `gosu` down
/// to a service user. `ply import` marks images with `capabilities = "oci"`
/// so they get exactly this and nothing more; ply-native packages keep the
/// empty default.
pub const OCI_DEFAULT_CAPABILITIES: [Capability; 14] = [
    Capability::CAP_CHOWN,
    Capability::CAP_DAC_OVERRIDE,
    Capability::CAP_FSETID,
    Capability::CAP_FOWNER,
    Capability::CAP_MKNOD,
    Capability::CAP_NET_RAW,
    Capability::CAP_SETGID,
    Capability::CAP_SETUID,
    Capability::CAP_SETFCAP,
    Capability::CAP_SETPCAP,
    Capability::CAP_NET_BIND_SERVICE,
    Capability::CAP_SYS_CHROOT,
    Capability::CAP_KILL,
    Capability::CAP_AUDIT_WRITE,
];

/// One `[package] capabilities` entry to a `Capability`. Case-insensitive,
/// `CAP_` optional — `chown`, `CHOWN` and `CAP_CHOWN` all land on the same
/// one, because nobody should have to remember which spelling ply wants.
pub fn parse_capability(name: &str) -> Result<Capability> {
    let upper = name.trim().to_ascii_uppercase();
    let full = if upper.starts_with("CAP_") {
        upper
    } else {
        format!("CAP_{upper}")
    };
    full.parse::<Capability>().map_err(|_| {
        Error::Manifest(format!(
            "unknown capability `{name}` in package.capabilities (expected e.g. \"chown\", \
             \"setuid\", \"net_bind_service\", or the preset \"oci\")"
        ))
    })
}

/// The capability set an app keeps: whatever its manifest asks for, plus
/// CAP_NET_BIND_SERVICE when it declares a privileged port (a declared port
/// is a promise ply keeps without making the author spell it twice).
pub fn keep_set(
    declared: Option<&crate::manifest::Capabilities>,
    keep_net_bind: bool,
) -> Result<Vec<Capability>> {
    use crate::manifest::Capabilities;
    let mut keep = match declared {
        None => Vec::new(),
        Some(Capabilities::Preset(name)) if name.eq_ignore_ascii_case("oci") => {
            OCI_DEFAULT_CAPABILITIES.to_vec()
        }
        Some(Capabilities::Preset(name)) => {
            return Err(Error::Manifest(format!(
                "unknown capabilities preset `{name}` — the only preset is \"oci\" \
                 (Docker's default set); otherwise list the capabilities explicitly"
            )))
        }
        Some(Capabilities::List(names)) => names
            .iter()
            .map(|n| parse_capability(n))
            .collect::<Result<Vec<_>>>()?,
    };
    if keep_net_bind && !keep.contains(&Capability::CAP_NET_BIND_SERVICE) {
        keep.push(Capability::CAP_NET_BIND_SERVICE);
    }
    Ok(keep)
}

/// Drop every capability from every set except `keep` in the bounding set.
/// After execve, root's permitted set is recomputed from bounding — so a
/// shrunken bounding set is what actually constrains the app.
pub fn drop_capabilities(keep: &[Capability]) -> Result<()> {
    for cap in caps::all() {
        if keep.contains(&cap) {
            continue;
        }
        caps::drop(None, CapSet::Bounding, cap)
            .map_err(|e| Error::Runtime(format!("drop bounding {cap}: {e}")))?;
    }
    caps::clear(None, CapSet::Ambient)
        .map_err(|e| Error::Runtime(format!("clear ambient caps: {e}")))?;
    Ok(())
}

pub fn no_new_privs() -> Result<()> {
    nix::sys::prctl::set_no_new_privs().map_err(|e| Error::Runtime(format!("no_new_privs: {e}")))
}

/// Seccomp blocklist: the syscalls Docker's default profile denies that
/// matter most (mount tree, ptrace, kernel modules, kexec, bpf, keyring,
/// namespaces, raw memory of other processes). Blocked = EPERM.
///
/// v1 is a blocklist (new/unlisted syscalls stay allowed); adopting the full
/// Docker allowlist is a planned tightening.
pub fn apply_seccomp() -> Result<()> {
    use nix::libc;
    // libc's aarch64-musl bindings lack SYS_kexec_file_load (arm64 syscall 294)
    #[cfg(target_arch = "x86_64")]
    const SYS_KEXEC_FILE_LOAD: i64 = libc::SYS_kexec_file_load;
    #[cfg(target_arch = "aarch64")]
    const SYS_KEXEC_FILE_LOAD: i64 = 294;
    let blocked: Vec<i64> = vec![
        libc::SYS_acct,
        libc::SYS_add_key,
        libc::SYS_bpf,
        libc::SYS_clock_adjtime,
        libc::SYS_clock_settime,
        libc::SYS_delete_module,
        libc::SYS_finit_module,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_fsopen,
        libc::SYS_fspick,
        libc::SYS_get_mempolicy,
        libc::SYS_init_module,
        libc::SYS_kcmp,
        SYS_KEXEC_FILE_LOAD,
        libc::SYS_kexec_load,
        libc::SYS_keyctl,
        libc::SYS_mbind,
        libc::SYS_mount,
        libc::SYS_move_mount,
        libc::SYS_move_pages,
        libc::SYS_name_to_handle_at,
        libc::SYS_open_by_handle_at,
        libc::SYS_open_tree,
        libc::SYS_perf_event_open,
        libc::SYS_pivot_root,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_ptrace,
        libc::SYS_quotactl,
        libc::SYS_reboot,
        libc::SYS_request_key,
        libc::SYS_set_mempolicy,
        libc::SYS_setns,
        libc::SYS_settimeofday,
        libc::SYS_swapoff,
        libc::SYS_swapon,
        libc::SYS_umount2,
        libc::SYS_unshare,
        libc::SYS_userfaultfd,
    ];

    let rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> =
        blocked.into_iter().map(|nr| (nr, vec![])).collect();

    let filter = seccompiler::SeccompFilter::new(
        rules,
        // unlisted syscalls run normally
        seccompiler::SeccompAction::Allow,
        // listed syscalls fail with EPERM (loud, not lethal)
        seccompiler::SeccompAction::Errno(nix::libc::EPERM as u32),
        std::env::consts::ARCH
            .try_into()
            .map_err(|_| Error::Runtime("unsupported seccomp arch".into()))?,
    )
    .map_err(|e| Error::Runtime(format!("seccomp filter build: {e}")))?;
    let program: seccompiler::BpfProgram = filter
        .try_into()
        .map_err(|e: seccompiler::BackendError| Error::Runtime(format!("seccomp compile: {e}")))?;
    seccompiler::apply_filter(&program)
        .map_err(|e| Error::Runtime(format!("seccomp apply: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod cap_tests {
    use super::*;
    use crate::manifest::Capabilities;

    #[test]
    fn native_packages_keep_nothing() {
        assert!(keep_set(None, false).unwrap().is_empty());
    }

    #[test]
    fn a_privileged_port_still_earns_net_bind_without_a_declaration() {
        assert_eq!(
            keep_set(None, true).unwrap(),
            vec![Capability::CAP_NET_BIND_SERVICE]
        );
    }

    #[test]
    fn oci_preset_is_dockers_set_exactly() {
        let keep = keep_set(Some(&Capabilities::Preset("oci".into())), false).unwrap();
        assert_eq!(keep.len(), 14, "Docker's default set is 14 capabilities");
        // the three the redis/postgres/nginx entrypoint pattern actually needs
        for needed in [
            Capability::CAP_CHOWN,
            Capability::CAP_SETUID,
            Capability::CAP_SETGID,
        ] {
            assert!(keep.contains(&needed), "{needed} missing");
        }
        // and the ones Docker withholds stay withheld
        for denied in [
            Capability::CAP_SYS_ADMIN,
            Capability::CAP_SYS_MODULE,
            Capability::CAP_SYS_PTRACE,
            Capability::CAP_NET_ADMIN,
        ] {
            assert!(!keep.contains(&denied), "{denied} must NOT be granted");
        }
        assert!(keep_set(Some(&Capabilities::Preset("OCI".into())), false).is_ok());
    }

    #[test]
    fn net_bind_is_not_duplicated_when_the_preset_already_has_it() {
        let keep = keep_set(Some(&Capabilities::Preset("oci".into())), true).unwrap();
        assert_eq!(
            keep.iter()
                .filter(|c| **c == Capability::CAP_NET_BIND_SERVICE)
                .count(),
            1
        );
    }

    #[test]
    fn explicit_lists_accept_every_spelling() {
        let keep = keep_set(
            Some(&Capabilities::List(vec![
                "chown".into(),
                "CAP_SETUID".into(),
                "Net_Bind_Service".into(),
            ])),
            false,
        )
        .unwrap();
        assert_eq!(
            keep,
            vec![
                Capability::CAP_CHOWN,
                Capability::CAP_SETUID,
                Capability::CAP_NET_BIND_SERVICE
            ]
        );
    }

    #[test]
    fn unknown_names_and_presets_are_refused_by_name() {
        let err = keep_set(Some(&Capabilities::List(vec!["sudo".into()])), false).unwrap_err();
        assert!(err.to_string().contains("sudo"), "{err}");
        let err = keep_set(Some(&Capabilities::Preset("docker".into())), false).unwrap_err();
        assert!(err.to_string().contains("docker"), "{err}");
        assert!(
            err.to_string().contains("oci"),
            "names the valid preset: {err}"
        );
    }

    /// `manifest::LINUX_CAPABILITY_NAMES` is a hand-copied stand-in for this
    /// crate's own `Capability` enum, used to validate `[package]
    /// capabilities` on platforms that don't have `caps` as a dependency
    /// (see `ply-core/Cargo.toml`) — everywhere but Linux. This is the one
    /// place that copy is checked against the source of truth (`caps::all()`)
    /// so it can never silently drift; deliberately here rather than next to
    /// the table in `manifest.rs`, because `caps` is a Linux-only dependency
    /// of this crate and the only files that may name it are `runtime/ns/*`
    /// and `craft.rs` — so the name table lives in `manifest.rs` and this
    /// crate-parity test lives here.
    #[test]
    fn capability_table_matches_the_caps_crate() {
        let mut from_table: Vec<String> = crate::manifest::LINUX_CAPABILITY_NAMES
            .iter()
            .map(|s| s.to_string())
            .collect();
        from_table.sort();

        let mut from_crate: Vec<String> = caps::all().iter().map(|c| format!("{c:?}")).collect();
        from_crate.sort();

        assert_eq!(
            from_table, from_crate,
            "manifest::LINUX_CAPABILITY_NAMES has drifted from caps::Capability's own variants"
        );
    }
}
