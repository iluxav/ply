use anyhow::{bail, Result};
use ply_core::runtime::state::{self, InstanceState};

use crate::cli::ProxyArgs;

/// `ply proxy [APP]… --format caddy|nginx|haproxy` — emit reverse-proxy
/// config to stdout. We never build a proxy; we emit config for boring
/// existing tools. No APP = every running app.
pub fn proxy(args: ProxyArgs) -> Result<()> {
    let render = match args.format.as_str() {
        "caddy" => caddy,
        "nginx" => nginx,
        "haproxy" => haproxy,
        other => bail!("unknown format `{other}` — supported: caddy, nginx, haproxy"),
    };

    let apps = if args.apps.is_empty() {
        let states = state::list()?;
        let mut apps: Vec<String> = states
            .iter()
            .filter(|s| s.alive())
            .map(|s| s.app.clone())
            .collect();
        apps.sort();
        apps.dedup();
        if apps.is_empty() {
            bail!("no running instances");
        }
        apps
    } else {
        args.apps.clone()
    };

    // Naming an app is a question about that app: answer it or fail. A sweep
    // is a question about the host, and one unpublishable app must not cost
    // you the config for every other one — say so on stderr and carry on.
    let sweeping = args.apps.is_empty();
    let mut emitted = 0usize;
    for app in &apps {
        let backends = match alive_backends(app) {
            Ok(b) if !b.is_empty() => b,
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
        print!("{}", render(app, &backends));
        emitted += 1;
    }
    if emitted == 0 {
        bail!("nothing to emit — no running app has an address a proxy can reach");
    }
    Ok(())
}

/// Where a proxy should send traffic for `app`.
///
/// A published app answers on ONE address — its run parent — which already
/// balances across instances, skips unhealthy ones and drains on deploy. That
/// address is what belongs in the config: it survives scale, rolls, crashes
/// and restarts, so the emitted file never needs regenerating for them.
///
/// Only an unpublished app falls back to per-instance addresses. That path is
/// rootful-only by nature: rootless instances share the host netns, so they
/// all report `127.0.0.1` and the manifest's declared port, which would emit
/// N identical, wrong backends.
fn alive_backends(app: &str) -> Result<Vec<String>> {
    let alive: Vec<InstanceState> = state::list()?
        .into_iter()
        .filter(|s| s.app == app && s.alive())
        .collect();

    if let Some(addr) = alive.iter().find_map(|s| s.published_addr.clone()) {
        return Ok(vec![addr]);
    }

    let rootless = !ply_core::paths::is_root();
    if rootless && alive.len() > 1 {
        bail!(
            "`{app}` runs {} rootless instances without --publish: they share the host network, \
             so there is no per-instance address to emit.\n\
             Publish the pool and the parent becomes the single backend: \
             ply run --publish internal:<port> --scale N …",
            alive.len()
        );
    }
    Ok(alive
        .iter()
        .filter_map(|s| first_port(s).map(|p| format!("{}:{p}", s.ip)))
        .collect())
}

fn first_port(s: &InstanceState) -> Option<u16> {
    s.ports.values().next().copied()
}

fn caddy(app: &str, backends: &[String]) -> String {
    format!("{app}.ply {{\n\treverse_proxy {}\n}}\n", backends.join(" "))
}

fn nginx(app: &str, backends: &[String]) -> String {
    let servers: Vec<String> = backends
        .iter()
        .map(|b| format!("    server {b};"))
        .collect();
    format!(
        "upstream {app} {{\n{}\n}}\n\nserver {{\n    listen 80;\n    server_name {app}.ply;\n    location / {{\n        proxy_pass http://{app};\n    }}\n}}\n",
        servers.join("\n")
    )
}

fn haproxy(app: &str, backends: &[String]) -> String {
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
            caddy("web", &backends),
            "web.ply {\n\treverse_proxy 10.77.0.1:3000\n}\n"
        );
        assert!(nginx("web", &backends).contains("server 10.77.0.1:3000;"));
        assert!(haproxy("web", &backends).contains("server web0 10.77.0.1:3000 check"));
    }

    #[test]
    fn a_published_app_emits_one_stable_backend() {
        // The parent balances, so the config holds a single address that
        // survives scale and rolls — not N instance IPs that go stale.
        let backends = vec!["10.77.0.1:3000".to_string()];
        let out = caddy("web", &backends);
        assert_eq!(out.matches("10.77.0.1:3000").count(), 1);
        assert!(!out.contains("10.77.0.2"), "no instance IPs: {out}");
    }
}
