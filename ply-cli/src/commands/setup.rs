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
//!
//! Everything here — AppArmor, subuid ranges, sysctls, systemd units — is
//! Linux host administration; there is nothing to prepare on a platform
//! `ply run` doesn't run on yet.

#[cfg(target_os = "linux")]
mod linux {
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
        if let Some(size) = &args.swap {
            swap_step(size)?;
        }
        if args.edge {
            edge_step()?;
        }
        if let Some(repo) = &args.fleet {
            fleet_step(repo, args.fleet_host.as_deref(), args.fleet_key.as_deref())?;
        }
        report_rootless_readiness(args.unprivileged_ports);
        Ok(())
    }

    /// `--swap 2G`: the classic small-VPS move, wrapped. Disk standing in as
    /// slow overflow memory — what lets a 512 MB droplet finish a 2 GB build.
    fn swap_step(size: &str) -> Result<()> {
        if std::fs::read_to_string("/proc/swaps")
            .map(|s| s.lines().count() > 1)
            .unwrap_or(false)
        {
            println!("ok: swap already active (/proc/swaps)");
            return Ok(());
        }
        size.strip_suffix(['G', 'M'])
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| anyhow::anyhow!("--swap `{size}`: expected e.g. 2G or 512M"))?;
        println!("creating /swapfile ({size}) …");
        run_cmd("fallocate", &["-l", size, "/swapfile"])?;
        std::fs::set_permissions(
            "/swapfile",
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )?;
        run_cmd("mkswap", &["/swapfile"])?;
        run_cmd("swapon", &["/swapfile"])?;
        let fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
        if !fstab.contains("/swapfile") {
            std::fs::write(
                "/etc/fstab",
                format!(
                    "{fstab}/swapfile none swap sw 0 0
"
                ),
            )?;
        }
        // overflow tier, not a habit: prefer RAM until pressure is real
        let _ = std::fs::write("/proc/sys/vm/swappiness", "10");
        let _ = std::fs::write(
            "/etc/sysctl.d/60-ply-swappiness.conf",
            "vm.swappiness=10
",
        );
        println!("swap active: {size} at /swapfile (persisted; swappiness 10)");
        Ok(())
    }

    /// `--edge`: Caddy + a ply-managed config + two units. After this, an app's
    /// `--domain` is the only step between it and HTTPS: the watcher renders
    /// vhosts from instance state; Caddy fetches certificates.
    const CADDY_VERSION: &str = "2.11.4";

    fn edge_step() -> Result<()> {
        std::fs::create_dir_all("/etc/ply/edge/apps")?;

        // 1. Caddy binary (skip when the host already has one).
        let caddy = find_in_path("caddy")
            .map(Ok)
            .unwrap_or_else(install_caddy)?;

        // 2. Root Caddyfile: imports the watcher's output. Only ever written by
        //    us — refuse to clobber a hand-edited one.
        const MARKER: &str = "# managed by `ply setup --edge`";
        let root = std::path::Path::new(crate::commands::lb::EDGE_CADDYFILE);
        let desired = format!(
            "{MARKER} — edit apps via `ply run --domain`, not here
import {}/apps/*.caddy
",
            crate::commands::lb::EDGE_DIR
        );
        let current = std::fs::read_to_string(root).unwrap_or_default();
        if current.is_empty() || current.starts_with(MARKER) {
            if current != desired {
                std::fs::write(root, &desired)?;
            }
            println!("ok: {} (imports apps/*.caddy)", root.display());
        } else {
            bail!(
                "{} exists and was not written by ply — move it aside and re-run",
                root.display()
            );
        }
        // The import glob must match something or Caddy refuses to start.
        let apps_file = std::path::Path::new(crate::commands::lb::EDGE_APPS_FILE);
        if !apps_file.exists() {
            std::fs::write(
                apps_file,
                "# ply proxy --watch writes app vhosts here
",
            )?;
        }

        // 3. Units: Caddy, and the watcher that feeds it.
        let ply = std::env::current_exe()?;
        let ply = ply.canonicalize().unwrap_or(ply);
        write_unit(
            "/etc/systemd/system/ply-edge.service",
            &format!(
                "[Unit]
Description=ply edge (Caddy)
After=network-online.target
Wants=network-online.target

             [Service]
ExecStart={caddy} run --config {config}
ExecReload={caddy} reload --config {config}
Restart=on-failure
LimitNOFILE=1048576

             [Install]
WantedBy=multi-user.target
",
                caddy = caddy,
                config = crate::commands::lb::EDGE_CADDYFILE,
            ),
        )?;
        write_unit(
            "/etc/systemd/system/ply-proxy.service",
            &format!(
                "[Unit]
Description=ply proxy watcher (renders app vhosts for the edge)
After=ply-edge.service
Wants=ply-edge.service

             [Service]
ExecStart={} proxy --watch
Restart=on-failure

             [Install]
WantedBy=multi-user.target
",
                ply.display()
            ),
        )?;
        automation_units(&ply)?;
        run_cmd("systemctl", &["daemon-reload"])?;
        run_cmd(
            "systemctl",
            &[
                "enable",
                "--now",
                "ply-edge",
                "ply-proxy",
                "ply-deployments.path",
                "ply-reconcile.timer",
                "ply-selfupdate.timer",
            ],
        )?;

        println!("edge installed: ply-edge (Caddy) + ply-proxy (watcher) are running");
        println!("deployments: drop a <name>.toml in /var/lib/ply/deployments and it runs");
        println!("next: point a domain's DNS at this host, open ports 80/443, then");
        println!("      ply run app.img --publish internal:<port> --domain app.example.com");
        Ok(())
    }

    fn find_in_path(bin: &str) -> Option<String> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
            .map(|p| p.display().to_string())
    }

    /// Download the official static Caddy build (the same one the caddy package
    /// in the registry wraps) to /usr/local/bin.
    fn install_caddy() -> Result<String> {
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            _ => "amd64",
        };
        let url = format!(
            "https://github.com/caddyserver/caddy/releases/download/v{CADDY_VERSION}/caddy_{CADDY_VERSION}_linux_{arch}.tar.gz"
        );
        println!("downloading caddy {CADDY_VERSION} ({arch}) …");
        let status = std::process::Command::new("sh")
            .args([
                "-c",
                &format!("curl -fsSL '{url}' | tar -xz -C /usr/local/bin caddy && chmod 755 /usr/local/bin/caddy"),
            ])
            .status()?;
        if !status.success() {
            bail!("caddy download failed ({url})");
        }
        Ok("/usr/local/bin/caddy".to_string())
    }

    fn write_unit(path: &str, content: &str) -> Result<()> {
        if std::fs::read_to_string(path).unwrap_or_default() != content {
            std::fs::write(path, content)?;
        }
        Ok(())
    }

    fn run_cmd(cmd: &str, args: &[&str]) -> Result<()> {
        let status = std::process::Command::new(cmd).args(args).status()?;
        if !status.success() {
            bail!("{cmd} {} exited with {status}", args.join(" "));
        }
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
                .and_then(|t| ply_core::runtime::ns::subid::parse_subid(&t, &user, id))
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

    /// The no-daemon automation stack shared by --edge and --fleet: the
    /// deployments watcher, the reconcile cadence, and platform self-update.
    fn automation_units(ply: &std::path::Path) -> Result<()> {
        // Declarative deployments: the dir is watched by systemd (inotify) and
        // `ply reconcile` converges units to it. A deployment is a file.
        std::fs::create_dir_all("/var/lib/ply/deployments")?;
        write_unit(
            "/etc/systemd/system/ply-deployments.path",
            "[Unit]
Description=ply deployments dir watcher (a deployment is a file)

         [Path]
PathModified=/var/lib/ply/deployments
MakeDirectory=yes

         [Install]
WantedBy=multi-user.target
",
        )?;
        write_unit(
            "/etc/systemd/system/ply-deployments.service",
            &format!(
                "[Unit]
Description=ply reconcile (converge systemd units to the deployments dir)

             [Service]
Type=oneshot
ExecStart={} reconcile
",
                ply.display()
            ),
        )?;
        // The cadence: follow-latest deployments converge on their own — push
        // code or tag a release and the host self-updates within a minute,
        // still zero resident processes (a timer is a clock, not a daemon).
        write_unit(
            "/etc/systemd/system/ply-reconcile.timer",
            "[Unit]
Description=ply reconcile cadence (follow-latest deployments self-update)

         [Timer]
OnBootSec=2min
OnUnitActiveSec=1min
Unit=ply-deployments.service

         [Install]
WantedBy=timers.target
",
        )?;
        // The platform keeps itself current too: a daily, jittered
        // self-update. Running apps are never restarted by it — `ply ps`
        // marks stale supervisors, and each app's next roll absorbs the
        // update.
        write_unit(
            "/etc/systemd/system/ply-selfupdate.service",
            &format!(
                "[Unit]
Description=ply self-update (track the newest release)

             [Service]
Type=oneshot
ExecStart={} self-update
",
                ply.display()
            ),
        )?;
        write_unit(
            "/etc/systemd/system/ply-selfupdate.timer",
            "[Unit]
Description=ply self-update cadence (daily, jittered)

         [Timer]
OnCalendar=daily
RandomizedDelaySec=1h
Persistent=true

         [Install]
WantedBy=timers.target
",
        )?;
        Ok(())
    }

    /// GitOps fleet: this host follows a git repo of deployment files.
    fn fleet_step(repo: &str, host: Option<&str>, key: Option<&str>) -> Result<()> {
        let ply = std::env::current_exe()?;
        let host = match host {
            Some(h) => h.to_string(),
            None => nix::unistd::gethostname()
                .map(|h| h.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        if host.is_empty() {
            anyhow::bail!("cannot determine a hostname — pass --fleet-host");
        }
        std::fs::create_dir_all("/etc/ply")?;
        let mut config = format!("repo = {repo:?}\nhost = {host:?}\n");
        if let Some(key) = key {
            config.push_str(&format!("deploy_key = {key:?}\n"));
        }
        std::fs::write("/etc/ply/fleet.toml", config)?;
        automation_units(&ply)?;
        run_cmd("systemctl", &["daemon-reload"])?;
        run_cmd(
            "systemctl",
            &[
                "enable",
                "--now",
                "ply-deployments.path",
                "ply-reconcile.timer",
                "ply-selfupdate.timer",
            ],
        )?;
        // first sync now, not in a minute
        run_cmd("systemctl", &["start", "ply-deployments.service"])?;
        println!("fleet: this host follows {repo} (hosts/{host}/ + shared/)");
        println!("fleet: git-managed deployments sync every reconcile beat; local files coexist");
        println!("fleet: sync state lands in deployments/.status/fleet.json");
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub use linux::exec;

#[cfg(not(target_os = "linux"))]
pub fn exec(_args: crate::cli::SetupArgs) -> anyhow::Result<()> {
    anyhow::bail!(
        "ply setup is Linux-only (subuid ranges, AppArmor, privileged ports); nothing to set up here"
    )
}
