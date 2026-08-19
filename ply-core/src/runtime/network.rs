//! Mode B networking: netns per instance, veth pair onto a ply0 bridge,
//! 10.77.0.0/16. Each instance really binds its declared port on its own IP.
//!
//! v1 shells out to iproute2 (`ip`, `nsenter`) — present on every server
//! Linux; native netlink is a later swap that changes nothing user-visible.

use std::net::Ipv4Addr;
use std::process::Command;

use crate::error::{Error, Result};

pub const BRIDGE: &str = "ply0";
pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 1);

fn sh(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| Error::Runtime(format!("{program} not found: {e} (iproute2 required)")))?;
    if !output.status.success() {
        return Err(Error::Runtime(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Create ply0 bridge with the gateway address. Idempotent.
pub fn ensure_bridge() -> Result<()> {
    if !std::path::Path::new("/sys/class/net").join(BRIDGE).exists() {
        sh("ip", &["link", "add", BRIDGE, "type", "bridge"])?;
    }
    // addr add fails with EEXIST if already there — tolerate
    let _ = Command::new("ip")
        .args(["addr", "add", &format!("{GATEWAY}/16"), "dev", BRIDGE])
        .output();
    sh("ip", &["link", "set", BRIDGE, "up"])
}

/// Lowest free 10.77.x.y outside {.0.0, .0.1}.
pub fn allocate_ip(used: &[Ipv4Addr]) -> Result<Ipv4Addr> {
    for third in 0..=255u8 {
        for fourth in 0..=255u8 {
            if third == 0 && fourth < 2 {
                continue;
            }
            let candidate = Ipv4Addr::new(10, 77, third, fourth);
            if !used.contains(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(Error::Runtime("10.77.0.0/16 exhausted".into()))
}

/// Wire the instance: veth pair, host end on the bridge, container end as
/// eth0 with `ip`/16 + default route via the gateway. The veth pair dies
/// with the netns — no teardown needed.
pub fn setup_instance(pid: i32, ip: Ipv4Addr) -> Result<()> {
    let octets = ip.octets();
    let host_if = format!("ply{:02x}{:02x}", octets[2], octets[3]);
    let cont_if = format!("{host_if}c");
    let pid_s = pid.to_string();

    sh(
        "ip",
        &[
            "link", "add", &host_if, "type", "veth", "peer", "name", &cont_if,
        ],
    )?;
    sh("ip", &["link", "set", &host_if, "master", BRIDGE, "up"])?;
    sh("ip", &["link", "set", &cont_if, "netns", &pid_s])?;

    let ns = |args: &[&str]| -> Result<()> {
        let mut full = vec!["-t", &pid_s, "-n", "ip"];
        full.extend_from_slice(args);
        sh("nsenter", &full)
    };
    ns(&["link", "set", "lo", "up"])?;
    ns(&["link", "set", &cont_if, "name", "eth0"])?;
    ns(&["addr", "add", &format!("{ip}/16"), "dev", "eth0"])?;
    ns(&["link", "set", "eth0", "up"])?;
    ns(&["route", "add", "default", "via", &GATEWAY.to_string()])?;
    Ok(())
}
