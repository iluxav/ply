use anyhow::{bail, Result};
use ply_core::runtime::state::{self, InstanceState};

use crate::cli::{LbArgs, ProxyArgs};

/// `ply lb <app> --format nginx|haproxy|caddy` — emit LB config to stdout.
/// We never build a proxy; we emit config for boring existing tools.
pub fn exec(args: LbArgs) -> Result<()> {
    let backends = alive_backends(&args.app)?;
    if backends.is_empty() {
        bail!(
            "no running instances of `{}` — start some with `ply run --scale N`",
            args.app
        );
    }
    match args.format.as_str() {
        "nginx" => print!("{}", nginx(&args.app, &backends)),
        "haproxy" => print!("{}", haproxy(&args.app, &backends)),
        "caddy" => print!("{}", caddy(&args.app, &backends)),
        other => bail!("unknown format `{other}` — supported: nginx, haproxy, caddy"),
    }
    Ok(())
}

/// `ply proxy --backend caddy` — one config for every running app.
pub fn proxy(args: ProxyArgs) -> Result<()> {
    if args.backend != "caddy" {
        bail!(
            "backend `{}` not supported yet — caddy is (single binary, auto TLS, API reload); for nginx/haproxy per app use `ply lb <app> --format …`",
            args.backend
        );
    }
    let states = state::list()?;
    let mut apps: Vec<String> = states.iter().map(|s| s.app.clone()).collect();
    apps.sort();
    apps.dedup();
    if apps.is_empty() {
        bail!("no running instances");
    }
    for app in apps {
        print!("{}", caddy(&app, &alive_backends(&app)?));
    }
    Ok(())
}

fn alive_backends(app: &str) -> Result<Vec<String>> {
    Ok(state::list()?
        .into_iter()
        .filter(|s| s.app == app && s.alive())
        .filter_map(|s| first_port(&s).map(|p| format!("{}:{p}", s.ip)))
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
