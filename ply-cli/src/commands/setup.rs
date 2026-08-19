//! `sudo ply setup` — idempotent one-shot host preparation.
//!
//! Today that means one thing: on kernels that restrict unprivileged user
//! namespaces (Ubuntu >= 24.04 AppArmor policy), install a profile granting
//! this binary `userns` so rootless `ply run` works. Other hosts: no-op.

use anyhow::{bail, Result};

const RESTRICT_SYSCTL: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";
const PROFILE_PATH: &str = "/etc/apparmor.d/ply";

pub fn exec() -> Result<()> {
    if !nix_is_root() {
        bail!("ply setup changes host config — run it as root: sudo ply setup");
    }

    let restricted = std::fs::read_to_string(RESTRICT_SYSCTL)
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    if !restricted {
        println!("ok: this kernel does not restrict unprivileged user namespaces — nothing to do");
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

    let current = std::fs::read_to_string(PROFILE_PATH).unwrap_or_default();
    if current == profile {
        println!("ok: AppArmor profile already installed ({PROFILE_PATH})");
        return Ok(());
    }
    std::fs::write(PROFILE_PATH, &profile)?;

    let status = std::process::Command::new("apparmor_parser")
        .args(["-r", PROFILE_PATH])
        .status();
    match status {
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

fn nix_is_root() -> bool {
    unsafe { libc_geteuid() == 0 }
}
extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}
