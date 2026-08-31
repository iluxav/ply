use std::path::Path;

use anyhow::{bail, Result};
use ply_core::runtime::state::{self, InstanceState};

use crate::cli::ProxyArgs;

/// Where the ply-managed edge keeps its config (installed by
/// `sudo ply setup --edge`). The watcher writes the apps file; Caddy's root
/// config imports it.
pub const EDGE_DIR: &str = "/etc/ply/edge";
pub const EDGE_CADDYFILE: &str = "/etc/ply/edge/Caddyfile";
pub const EDGE_APPS_FILE: &str = "/etc/ply/edge/apps/ply.caddy";

/// `ply proxy [APP]… --format caddy|nginx|haproxy [--watch --out FILE]` —
/// emit reverse-proxy config. We never build a proxy; we emit config for
/// boring existing tools. No APP = every running app. `--watch` keeps the
/// file current as apps come, go, scale and deploy, reloading Caddy on
/// change — that plus `--domain` is the HTTPS story.
pub fn proxy(args: ProxyArgs) -> Result<()> {
    let render = match args.format.as_str() {
        "caddy" => caddy,
        "nginx" => nginx,
        "haproxy" => haproxy,
        other => bail!("unknown format `{other}` — supported: caddy, nginx, haproxy"),
    };

    if args.watch {
        return watch(&args, render);
    }

    let sweeping = args.apps.is_empty();
    let (config, emitted) = render_apps(&args.apps, render, sweeping)?;
    if emitted == 0 {
        bail!("nothing to emit — no running app has an address a proxy can reach");
    }
    match &args.out {
        Some(path) => write_atomic(Path::new(path), &config)?,
        None => print!("{config}"),
    }
    Ok(())
}

/// The watch loop: regenerate on any state change, write only on difference,
/// reload Caddy after each write. Errors inside the loop are logged and
/// retried — the watcher outliving a hiccup is the whole point of it.
fn watch(args: &ProxyArgs, render: RenderFn) -> Result<()> {
    let out = args.out.clone().unwrap_or_else(|| EDGE_APPS_FILE.into());
    let out = Path::new(&out);
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    eprintln!(
        "ply: proxy watch — writing {} on change, reloading caddy",
        out.display()
    );
    let mut last: Option<String> = None;
    loop {
        match render_apps(&args.apps, render, true) {
            Ok((config, _)) => {
                if last.as_deref() != Some(config.as_str()) {
                    match write_atomic(out, &config) {
                        Ok(()) => {
                            eprintln!("ply: proxy config updated ({} bytes)", config.len());
                            last = Some(config);
                            reload_caddy();
                        }
                        Err(e) => eprintln!("ply: writing {}: {e}", out.display()),
                    }
                }
            }
            Err(e) => eprintln!("ply: proxy render: {e}"),
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

fn reload_caddy() {
    let status = std::process::Command::new("caddy")
        .args(["reload", "--config", EDGE_CADDYFILE])
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("ply: caddy reload exited with {s} — config written, reload it manually")
        }
        Err(e) => eprintln!(
            "ply: caddy reload failed ({e}) — is the edge installed? (sudo ply setup --edge)"
        ),
    }
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

type RenderFn = fn(&str, &[String], &[String]) -> String;

/// Render config for the named apps (empty = every running app). Sweeping
/// skips unpublishable apps with a note; naming an app makes its failure
/// fatal. Returns (config, apps emitted).
fn render_apps(selected: &[String], render: RenderFn, sweeping: bool) -> Result<(String, usize)> {
    let apps = if selected.is_empty() {
        let states = state::list()?;
        let mut apps: Vec<String> = states
            .iter()
            .filter(|s| s.alive())
            .map(|s| s.app.clone())
            .collect();
        apps.sort();
        apps.dedup();
        if apps.is_empty() && !sweeping {
            bail!("no running instances");
        }
        apps
    } else {
        selected.to_vec()
    };

    let mut config = String::new();
    let mut emitted = 0usize;
    for app in &apps {
        let (backends, domains) = match backends_and_domains(app) {
            Ok((b, d)) if !b.is_empty() => (b, d),
            Ok(_) if sweeping => {
                eprintln!("ply: skipping `{app}` — no running instances");
                continue;
            }
            Ok(_) => bail!("no running instances of `{app}` — start some with `ply run --scale N`"),
            Err(e) if sweeping => {
                eprintln!("ply: skipping `{app}` — {e}");
                continue;
            }
            Err(e) => return Err(e),
        };
        config.push_str(&render(app, &backends, &domains));
        emitted += 1;
    }
    Ok((config, emitted))
}

/// Where a proxy should send traffic for `app`, plus its `--domain` list.
///
/// A published app answers on ONE address — its run parent — which already
/// balances across instances, skips unhealthy ones and drains on deploy. That
/// address is what belongs in the config: it survives scale, rolls, crashes
/// and restarts, so the emitted file never needs regenerating for them.
///
/// Only an unpublished app falls back to per-instance addresses. That path is
/// rootful-only by nature: rootless instances share their run's namespace,
/// so they all report `127.0.0.1` and the manifest's declared port — N
/// identical backends naming an address nothing on the host can reach.
fn backends_and_domains(app: &str) -> Result<(Vec<String>, Vec<String>)> {
    let alive: Vec<InstanceState> = state::list()?
        .into_iter()
        .filter(|s| s.app == app && s.alive())
        .collect();

    let domains = alive
        .iter()
        .find(|s| !s.domains.is_empty())
        .map(|s| s.domains.clone())
        .unwrap_or_default();

    if let Some(addr) = alive.iter().find_map(|s| s.published_addr.clone()) {
        return Ok((vec![addr], domains));
    }

    let rootless = !ply_core::paths::is_root();
    if rootless && alive.len() > 1 {
        bail!(
            "`{app}` runs {} rootless instances without --publish: they share one \
             namespace, so there is no per-instance address to emit.\n\
             Publish the pool and the parent becomes the single backend: \
             ply run --publish internal:<port> --scale N …",
            alive.len()
        );
    }
    Ok((
        alive
            .iter()
            .filter_map(|s| first_port(s).map(|p| format!("{}:{p}", s.ip)))
            .collect(),
        domains,
    ))
}

fn first_port(s: &InstanceState) -> Option<u16> {
    s.ports.values().next().copied()
}

/// With domains, the vhost is the domain list and Caddy's automatic HTTPS
/// takes it from there. Without, the local `.ply` name (plain HTTP).
fn caddy(app: &str, backends: &[String], domains: &[String]) -> String {
    let host = if domains.is_empty() {
        format!("{app}.ply")
    } else {
        domains.join(", ")
    };
    format!("{host} {{\n\treverse_proxy {}\n}}\n", backends.join(" "))
}

fn nginx(app: &str, backends: &[String], domains: &[String]) -> String {
    let servers: Vec<String> = backends
        .iter()
        .map(|b| format!("    server {b};"))
        .collect();
    let names = if domains.is_empty() {
        format!("{app}.ply")
    } else {
        domains.join(" ")
    };
    format!(
        "upstream {app} {{\n{}\n}}\n\nserver {{\n    listen 80;\n    server_name {names};\n    location / {{\n        proxy_pass http://{app};\n    }}\n}}\n",
        servers.join("\n")
    )
}

fn haproxy(app: &str, backends: &[String], _domains: &[String]) -> String {
    let servers: Vec<String> = backends
        .iter()
        .enumerate()
        .map(|(i, b)| format!("    server {app}{i} {b} check"))
        .collect();
    format!(
        "frontend {app}_front\n    bind *:80\n    default_backend {app}_back\n\nbackend {app}_back\n    balance roundrobin\n{}\n",
        servers.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_format_renders_the_backends_it_was_given() {
        let backends = vec!["10.77.0.1:3000".to_string()];
        assert_eq!(
            caddy("web", &backends, &[]),
            "web.ply {\n\treverse_proxy 10.77.0.1:3000\n}\n"
        );
        assert!(nginx("web", &backends, &[]).contains("server 10.77.0.1:3000;"));
        assert!(haproxy("web", &backends, &[]).contains("server web0 10.77.0.1:3000 check"));
    }

    #[test]
    fn a_published_app_emits_one_stable_backend() {
        // The parent balances, so the config holds a single address that
        // survives scale and rolls — not N instance IPs that go stale.
        let backends = vec!["10.77.0.1:3000".to_string()];
        let out = caddy("web", &backends, &[]);
        assert_eq!(out.matches("10.77.0.1:3000").count(), 1);
        assert!(!out.contains("10.77.0.2"), "no instance IPs: {out}");
    }

    #[test]
    fn domains_become_the_vhost() {
        let backends = vec!["10.77.0.1:3000".to_string()];
        let domains = vec!["api.example.com".to_string(), "www.example.com".to_string()];
        assert_eq!(
            caddy("web", &backends, &domains),
            "api.example.com, www.example.com {\n\treverse_proxy 10.77.0.1:3000\n}\n"
        );
        let n = nginx("web", &backends, &domains);
        assert!(n.contains("server_name api.example.com www.example.com;"));
        // no domains → the local .ply name, unchanged behavior
        assert!(caddy("web", &backends, &[]).starts_with("web.ply {"));
    }
}
