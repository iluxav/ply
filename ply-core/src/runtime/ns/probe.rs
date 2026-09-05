//! What the autoscaler measures, per instance, from files the parent already
//! has: the instance's cgroup, the host side of its veth, and — for a custom
//! signal — one HTTP GET of its metrics endpoint. Raw counters in, rates
//! out; the policy in `crate::autoscale` never sees a file.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use crate::autoscale::Raw;
use crate::runtime::ns::cgroup;

pub fn veth_for(ip: Ipv4Addr) -> String {
    let o = ip.octets();
    format!("ply{:02x}{:02x}", o[2], o[3])
}

/// `cpu.max`: "QUOTA PERIOD" → millicores; "max PERIOD" → unlimited.
pub fn parse_cpu_max(text: &str) -> Option<u64> {
    let mut it = text.split_whitespace();
    let quota: u64 = it.next()?.parse().ok()?;
    let period: u64 = it.next()?.parse().ok()?;
    (period > 0).then(|| quota * 1000 / period)
}

fn read_u64(path: PathBuf) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn stat_field(path: PathBuf, field: &str) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .find(|l| l.starts_with(field))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

/// Everything the autoscaler can read about `app.n` right now.
pub fn read(app: &str, n: u32, ip: Ipv4Addr) -> Raw {
    let cg = cgroup::instance_dir(app, n);
    let veth = veth_for(ip);
    let net = |f: &str| {
        read_u64(PathBuf::from(format!(
            "/sys/class/net/{veth}/statistics/{f}"
        )))
    };
    Raw {
        cpu_usec: stat_field(cg.join("cpu.stat"), "usage_usec"),
        nr_throttled: stat_field(cg.join("cpu.stat"), "nr_throttled"),
        mem_current: read_u64(cg.join("memory.current")),
        mem_max: read_u64(cg.join("memory.max")), // "max" parses as None: unlimited
        oom_kill: stat_field(cg.join("memory.events"), "oom_kill"),
        net_bytes: match (net("rx_bytes"), net("tx_bytes")) {
            (Some(rx), Some(tx)) => Some(rx + tx),
            _ => None,
        },
        cpu_quota_m: std::fs::read_to_string(cg.join("cpu.max"))
            .ok()
            .and_then(|s| parse_cpu_max(&s)),
    }
}

pub fn http_get(path: &str, host: &str) -> String {
    format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n")
}

/// The body of a 200 response, or `None` for anything else.
pub fn body_of(response: &str) -> Option<&str> {
    let (head, body) = response.split_once("\r\n\r\n")?;
    let status = head.lines().next()?;
    status
        .split_whitespace()
        .nth(1)
        .filter(|c| c.starts_with('2'))?;
    Some(body)
}

/// One scrape: the metric `name` from `http://ip:port/path`, within `timeout`.
pub fn fetch_metric(
    ip: Ipv4Addr,
    port: u16,
    path: &str,
    name: &str,
    timeout: Duration,
) -> Option<f64> {
    let addr = SocketAddr::from((ip, port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    stream
        .write_all(http_get(path, &addr.to_string()).as_bytes())
        .ok()?;
    let mut buf = Vec::new();
    let _ = stream.take(4 << 20).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    crate::autoscale::prometheus_value(body_of(&text)?, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoscale::Reading;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    #[test]
    fn the_veth_name_is_the_bridge_ip_in_hex() {
        assert_eq!(veth_for(Ipv4Addr::new(10, 77, 0, 2)), "ply0002");
        assert_eq!(veth_for(Ipv4Addr::new(10, 77, 1, 255)), "ply01ff");
    }

    #[test]
    fn cpu_max_reads_as_millicores_and_unlimited_as_none() {
        assert_eq!(parse_cpu_max("100000 100000\n"), Some(1000));
        assert_eq!(parse_cpu_max("150000 100000"), Some(1500));
        assert_eq!(parse_cpu_max("max 100000"), None);
        assert_eq!(parse_cpu_max("garbage"), None);
    }

    /// 2.5 s of CPU in 5 s is half a core; against a 2-core quota it is 25 %.
    #[test]
    fn rates_come_from_two_snapshots() {
        let a = Raw {
            cpu_usec: Some(1_000_000),
            nr_throttled: Some(3),
            mem_current: Some(300 << 20),
            mem_max: Some(1 << 30),
            oom_kill: Some(0),
            net_bytes: Some(1_000_000),
            cpu_quota_m: Some(2000),
        };
        let b = Raw {
            cpu_usec: Some(3_500_000),
            nr_throttled: Some(5),
            mem_current: Some(600 << 20),
            mem_max: Some(1 << 30),
            oom_kill: Some(1),
            net_bytes: Some(21_000_000),
            cpu_quota_m: Some(2000),
        };
        let r = Reading::between(&a, &b, Duration::from_secs(5));
        assert_eq!(r.cpu_pct_of_core, Some(50.0));
        assert_eq!(r.cpu_pct_of_quota, Some(25.0));
        // The quota grew mid-interval (vertical scaling): the usage was capped
        // by the OLD quota, so that is the one it is measured against.
        let grown = Raw {
            cpu_quota_m: Some(4000),
            ..b.clone()
        };
        assert_eq!(
            Reading::between(&a, &grown, Duration::from_secs(5)).cpu_pct_of_quota,
            Some(25.0)
        );
        assert_eq!(r.mem_pct, Some(600.0 / 1024.0 * 100.0));
        assert_eq!(r.net_bps, Some(4_000_000.0));
        assert!(r.throttled_grew);
        assert!(r.oom_grew);
        let none = Reading::between(&Raw::default(), &Raw::default(), Duration::from_secs(5));
        assert_eq!(none.cpu_pct_of_core, None);
        assert_eq!(none.mem_pct, None);
    }

    #[test]
    fn the_metrics_request_is_plain_http_1_0_and_the_body_follows_the_blank_line() {
        assert_eq!(
            http_get("/metrics", "10.77.0.3:8080"),
            "GET /metrics HTTP/1.0\r\nHost: 10.77.0.3:8080\r\nConnection: close\r\n\r\n"
        );
        let ok = "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\n\r\nqueue_depth 7\n";
        assert_eq!(body_of(ok), Some("queue_depth 7\n"));
        assert_eq!(body_of("HTTP/1.0 404 Not Found\r\n\r\nnope"), None);
        assert_eq!(body_of("garbage"), None);
    }
}
