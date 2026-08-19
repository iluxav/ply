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

/// Drop every capability from every set except `keep` in the bounding set.
/// After execve, root's permitted set is recomputed from bounding — so a
/// shrunken bounding set is what actually constrains the app.
pub fn drop_capabilities(keep_net_bind: bool) -> Result<()> {
    let keep = if keep_net_bind {
        vec![Capability::CAP_NET_BIND_SERVICE]
    } else {
        vec![]
    };
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
        libc::SYS_kexec_file_load,
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
