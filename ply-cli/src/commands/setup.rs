//! `sudo ply setup` — idempotent one-shot host preparation.
//!
//! Three things stand between a fresh host and a fully working rootless
//! `ply run`, and only the first is ply's to fix unprompted:
//!
//!   1. AppArmor `userns` permission (Ubuntu >= 24.04). Installed always —
//!      it grants this binary exactly what Docker and Chrome already have.
//!   2. A delegated `/etc/subuid` range plus `newuidmap`. Reported, never
//!      applied: it changes another account's id delegation.
//!   3. `net.ipv4.ip_unprivileged_port_start`, so a rootless instance can
//!      bind :80. Applied ONLY behind `--unprivileged-ports`: it lowers a
//!      host-wide boundary, which is the operator's call, not ply's.

use anyhow::{bail, Result};

use crate::cli::SetupArgs;

const RESTRICT_SYSCTL: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";
const PROFILE_PATH: &str = "/etc/apparmor.d/ply";
const PORT_START_SYSCTL: &str = "/proc/sys/net/ipv4/ip_unprivileged_port_start";
const PORT_START_CONF: &str = "/etc/sysctl.d/60-ply-unprivileged-ports.conf";

pub fn exec(args: SetupArgs) -> Result<()> {
    if !nix_is_root() {
        bail!("ply setup changes host config — run it as root: sudo ply setup");
    }

    apparmor_step()?;
    if let Some(port) = args.unprivileged_ports {
        unprivileged_ports_step(port)?;
    }
    report_rootless_readiness(args.unprivileged_ports);
    Ok(())
}

/// Step 1 — the profile that lets an unprivileged `ply` open a user namespace.
fn apparmor_step() -> Result<()> {
    let restricted = std::fs::read_to_string(RESTRICT_SYSCTL)
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    if !restricted {
        println!("ok: this kernel does not restrict unprivileged user namespaces");
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let profile = format!(
        "# installed by `ply setup` — allows rootless `ply run` on kernels that\n\
         # restrict unprivileged user namespaces (same requirement as Docker/Chrome)\n\
         abi <abi/4.0>,\n\
         include <tunables/global>\n\
         \n\
         profile ply {} flags=(unconfined) {{\n\
         \x20\x20userns,\n\
         }}\n",
        exe.display()
    );

    if std::fs::read_to_string(PROFILE_PATH).unwrap_or_default() == profile {
        println!("ok: AppArmor profile already installed ({PROFILE_PATH})");
        return Ok(());
    }
    std::fs::write(PROFILE_PATH, &profile)?;

    match std::process::Command::new("apparmor_parser")
        .args(["-r", PROFILE_PATH])
        .status()
    {
        Ok(s) if s.success() => {
            println!(
                "installed {PROFILE_PATH} for {} — rootless `ply run` enabled",
                exe.display()
            );
            Ok(())
        }
        Ok(s) => bail!("apparmor_parser -r {PROFILE_PATH} exited with {s}"),
        Err(e) => bail!("apparmor_parser not found ({e}) — is AppArmor installed?"),
    }
}

/// Step 3 — lower the privileged-port floor so rootless instances can bind :80.
///
/// Rootless shares the host netns, and CAP_NET_BIND_SERVICE inside a user
/// namespace does not authorize a privileged port out there. nginx, httpd,
/// caddy and traefik all bind :80 themselves, so without this they only run
/// rootful. Rootless docker/podman need the same knob.
fn unprivileged_ports_step(port: u16) -> Result<()> {
    let current: u16 = std::fs::read_to_string(PORT_START_SYSCTL)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(1024);

    let conf = format!(
        "# installed by `ply setup --unprivileged-ports {port}`\n\
         # lets unprivileged processes (rootless ply instances) bind ports >= {port}\n\
         net.ipv4.ip_unprivileged_port_start = {port}\n"
    );
    let already = std::fs::read_to_string(PORT_START_CONF).unwrap_or_default() == conf;

    if current <= port && already {
        println!("ok: unprivileged ports already start at {current} ({PORT_START_CONF})");
        return Ok(());
    }

    // Live first so the change takes effect now, then persist across reboots.
    std::fs::write(PORT_START_SYSCTL, format!("{port}\n"))
        .map_err(|e| anyhow::anyhow!("writing {PORT_START_SYSCTL}: {e}"))?;
    std::fs::write(PORT_START_CONF, &conf)
        .map_err(|e| anyhow::anyhow!("writing {PORT_START_CONF}: {e}"))?;

    println!(
        "set net.ipv4.ip_unprivileged_port_start = {port} (was {current}), persisted in {PORT_START_CONF}\n\
         note: this is host-wide — ANY unprivileged process may now bind ports >= {port}.\n\
         note: undo with: sudo rm {PORT_START_CONF} && sudo sysctl -w net.ipv4.ip_unprivileged_port_start=1024"
    );
    Ok(())
}

/// Step 2 (report only) plus a nudge toward step 3 when it was not requested.
fn report_rootless_readiness(ports_applied: Option<u16>) {
    let Some((user, uid, gid)) = invoking_user() else {
        return;
    };

    let range = |file: &str, id: u32| {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|t| ply_core::runtime::run::parse_subid(&t, &user, id))
    };
    let has_subids = range("/etc/subuid", uid).is_some() && range("/etc/subgid", gid).is_some();
    let has_helpers = which("newuidmap") && which("newgidmap");

    match (has_subids, has_helpers) {
        (true, true) => println!("ok: {user} has a subuid range and newuidmap — rootless `[package] user` and OCI imports work"),
        (false, _) => println!(
            "todo: {user} has no /etc/subuid + /etc/subgid range — rootless apps that switch user\n      \
             (`[package] user`, imported images that drop privileges) will fail with EINVAL.\n      \
             fix: sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 {user}"
        ),
        (true, false) => println!(
            "todo: newuidmap/newgidmap missing — {user} has a delegated subuid range but the kernel\n      \
             will not let an unprivileged process apply it without the setuid helpers.\n      \
             fix: sudo apt install uidmap   (or: dnf install shadow-utils)"
        ),
    }

    if ports_applied.is_none() {
        let current: u16 = std::fs::read_to_string(PORT_START_SYSCTL)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1024);
        if current > 80 {
            println!(
                "todo: unprivileged ports start at {current}, so rootless instances cannot bind :80\n      \
                 (nginx, httpd, caddy, traefik bind it themselves). Rootful is unaffected.\n      \
                 fix: sudo ply setup --unprivileged-ports"
            );
        }
    }
}

/// Who ran the sudo — the account rootless `ply run` will actually use.
fn invoking_user() -> Option<(String, u32, u32)> {
    let name = std::env::var("SUDO_USER").ok()?;
    let text = std::fs::read_to_string("/etc/passwd").ok()?;
    text.lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.len() > 3 && f[0] == name)
        .and_then(|f| Some((name.clone(), f[2].parse().ok()?, f[3].parse().ok()?)))
}

fn which(tool: &str) -> bool {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|d| !d.is_empty() && std::path::Path::new(d).join(tool).exists())
}

fn nix_is_root() -> bool {
    unsafe { libc_geteuid() == 0 }
}
extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}
