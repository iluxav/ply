//! Rootless id mapping: the invoking user's /etc/subuid range mapped into
//! the child's user namespace through newuidmap/newgidmap.
use std::path::Path;

use crate::error::{Error, Result};

/// A `/etc/subuid` or `/etc/subgid` delegation: `<name>:<start>:<count>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubIdRange {
    pub start: u32,
    pub count: u32,
}

/// This user's line in a subid file. Entries are keyed by name, but the
/// numeric id is accepted too — both spellings appear in the wild.
pub fn parse_subid(text: &str, user: &str, id: u32) -> Option<SubIdRange> {
    let id = id.to_string();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 3 || (f[0] != user && f[0] != id) {
            continue;
        }
        if let (Ok(start), Ok(count)) = (f[1].parse::<u32>(), f[2].parse::<u32>()) {
            if count > 0 {
                return Some(SubIdRange { start, count });
            }
        }
    }
    None
}

/// The invoking user's name, read from the host's passwd file.
/// Our own id map, rewritten so a child sees the same ids we do. An entry
/// `inside outside count` becomes `inside inside count`: the child's view
/// matches ours, and every id it names is one we actually hold.
///
/// Empty when we are the initial namespace (mapping everything), which is
/// not something to hand a child — the helpers deal with that case.
fn mirror_own_map(file: &str) -> String {
    let Ok(map) = std::fs::read_to_string(format!("/proc/self/{file}")) else {
        return String::new();
    };
    let mut out = String::new();
    for line in map.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() != 3 {
            return String::new();
        }
        if f == ["0", "0", "4294967295"] {
            return String::new(); // the initial namespace
        }
        out.push_str(&format!("{} {} {}\n", f[0], f[0], f[2]));
    }
    out
}

fn username_for(uid: u32) -> Option<String> {
    let text = std::fs::read_to_string("/etc/passwd").ok()?;
    text.lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.len() > 2 && f[2].parse::<u32>() == Ok(uid))
        .map(|f| f[0].to_string())
}

fn have(tool: &str) -> bool {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|d| !d.is_empty() && Path::new(d).join(tool).exists())
}

/// Map the invoking user into the child's user namespace.
///
/// Without a delegated subuid range only ONE id exists inside (root = you),
/// and every other uid is unmapped: `chown redis .` and `setuid(70)` both
/// fail with EINVAL, which breaks `[package] user` and every imported image
/// that drops privileges. A `/etc/subuid` range fixes that, but the kernel
/// only lets an unprivileged process write a single-entry map — writing a
/// range needs CAP_SETUID in the parent namespace, which is exactly what the
/// setuid-root `newuidmap`/`newgidmap` helpers are for. Same mechanism
/// rootless podman and docker use.
/// Map a child's user namespace from OUT HERE.
///
/// The child cannot map itself: AppArmor's `apparmor_restrict_unprivileged_userns`
/// (Ubuntu 24.04+) leaves an unprivileged user namespace without the
/// capabilities its own `uid_map`/`setgroups` writes need. The setuid
/// `newuidmap`/`newgidmap` helpers are how this is done everywhere, and how
/// ply has always done it for containers.
pub(crate) fn write_id_maps(pid: i32) -> Result<()> {
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    let user = username_for(uid).unwrap_or_default();

    let sub = |file: &str, id: u32| {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|t| parse_subid(&t, &user, id))
    };
    let ranges = match (sub("/etc/subuid", uid), sub("/etc/subgid", gid)) {
        (Some(u), Some(g)) if have("newuidmap") && have("newgidmap") => Some((u, g)),
        _ => None,
    };

    // Write the child's maps ourselves when we may. Inside a namespace ply
    // created (`ply up`'s fleet) we hold CAP_SETUID/CAP_SETGID over our
    // children, and the setuid helpers are powerless there anyway: host
    // root is unmapped, so their setuid bit does nothing and they fail with
    // EPERM. Outside, this write is refused and the helpers below run
    // exactly as before.
    //
    // The child gets OUR id space, one-to-one — the ids we may delegate are
    // precisely the ones mapped to us, and `/proc/self/uid_map` is the only
    // honest account of those.
    if let Some((umap, gmap)) = (mirror_own_map("uid_map"), mirror_own_map("gid_map")).into() {
        let direct = |file: &str, contents: &str| -> std::io::Result<()> {
            std::fs::write(format!("/proc/{pid}/{file}"), contents)
        };
        // NOT setgroups=deny: irreversible, and gosu/su-exec need setgroups
        // on their way down to a service user.
        if !umap.is_empty()
            && !gmap.is_empty()
            && direct("gid_map", &gmap).is_ok()
            && direct("uid_map", &umap).is_ok()
        {
            return Ok(());
        }
    }

    if let Some((u, g)) = ranges {
        // NOT setgroups=deny here: that is irreversible, and gosu/su-exec
        // call setgroups() on their way down to a service user. The helpers
        // are setuid-root, so they can write gid_map without it.
        let helper = |tool: &str, args: [String; 7]| -> Result<()> {
            let out = std::process::Command::new(tool)
                .args(&args)
                .output()
                .map_err(|e| Error::Runtime(format!("{tool}: {e}")))?;
            if !out.status.success() {
                return Err(Error::Runtime(format!(
                    "{tool} {}: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            Ok(())
        };
        // RootIsYou: inside 0 -> your uid (1 id), then inside 1.. -> the
        // delegated range. Identity: every id keeps its number, so the
        // subuid range is usable by nested namespaces under the same names.
        helper(
            "newuidmap",
            [
                pid.to_string(),
                "0".into(),
                uid.to_string(),
                "1".into(),
                "1".into(),
                u.start.to_string(),
                u.count.to_string(),
            ],
        )?;
        helper(
            "newgidmap",
            [
                pid.to_string(),
                "0".into(),
                gid.to_string(),
                "1".into(),
                "1".into(),
                g.start.to_string(),
                g.count.to_string(),
            ],
        )?;
        return Ok(());
    }

    // Fallback: the single-id map. Everything still runs except apps that
    // need a second uid to exist — say so once, with the fix.
    let write = |file: &str, contents: String| -> Result<()> {
        let path = format!("/proc/{pid}/{file}");
        std::fs::write(&path, contents).map_err(|source| Error::Io {
            path: path.into(),
            source,
        })
    };
    write("setgroups", "deny".into())?;
    write("gid_map", format!("0 {gid} 1"))?;
    write("uid_map", format!("0 {uid} 1"))?;
    Ok(())
}

/// Why the single-id map is in play, for the one-line warning at startup.
/// None when a delegated range is usable.
pub fn subid_gap() -> Option<&'static str> {
    // Inside a namespace ply owns, the range comes from the map we already
    // hold, not from /etc/subuid — where this process is uid 0 and would
    // look up "root" and find nothing. Warning there would be false.
    if !mirror_own_map("uid_map").is_empty() {
        return None;
    }
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    let user = username_for(uid).unwrap_or_default();
    let has_range = |file: &str, id: u32| {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|t| parse_subid(&t, &user, id))
            .is_some()
    };
    if !has_range("/etc/subuid", uid) || !has_range("/etc/subgid", gid) {
        return Some(
            "no /etc/subuid+/etc/subgid range for this user — only uid 0 exists inside, so \
             `[package] user` and imported images that drop privileges will fail with EINVAL.\n\
             ply:          fix: sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER",
        );
    }
    if !have("newuidmap") || !have("newgidmap") {
        return Some(
            "newuidmap/newgidmap not installed — a delegated subuid range exists but the kernel \
             will not let an unprivileged process apply it, so only uid 0 exists inside.\n\
             ply:          fix: sudo apt install uidmap   (or: dnf install shadow-utils)",
        );
    }
    None
}

#[cfg(test)]
mod subid_tests {
    use super::*;

    const SUBUID: &str = "# a comment
iluxa:100000:65536
someoneelse:200000:65536
";

    #[test]
    fn finds_the_range_for_this_user() {
        assert_eq!(
            parse_subid(SUBUID, "iluxa", 1000),
            Some(SubIdRange {
                start: 100000,
                count: 65536
            })
        );
    }

    #[test]
    fn matches_by_numeric_id_too() {
        // some distros write the uid instead of the name
        assert_eq!(
            parse_subid("1000:100000:65536\n", "", 1000),
            Some(SubIdRange {
                start: 100000,
                count: 65536
            })
        );
    }

    #[test]
    fn another_users_delegation_is_not_ours() {
        assert_eq!(parse_subid(SUBUID, "nobody", 65534), None);
    }

    #[test]
    fn junk_lines_are_skipped_not_fatal() {
        let text = "broken\n# comment\nnocolons\na:b:c\niluxa:100000:65536\n";
        assert_eq!(
            parse_subid(text, "iluxa", 1000),
            Some(SubIdRange {
                start: 100000,
                count: 65536
            })
        );
    }

    #[test]
    fn a_zero_width_delegation_is_no_delegation() {
        // a range of 0 ids maps nothing — treat it as absent rather than
        // handing newuidmap an argument it will reject
        assert_eq!(parse_subid("iluxa:100000:0\n", "iluxa", 1000), None);
    }

    #[test]
    fn the_range_covers_the_service_uids_that_matter() {
        let r = parse_subid(SUBUID, "iluxa", 1000).unwrap();
        // redis 999, nginx 101, postgres 70, memcached 11211 all land inside
        for uid in [70u32, 101, 999, 11211] {
            assert!(
                uid >= 1 && uid <= r.count,
                "uid {uid} outside 1..={}",
                r.count
            );
        }
    }
}
