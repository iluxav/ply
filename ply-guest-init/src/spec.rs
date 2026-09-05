//! Reading the spec disk and rendering the small config files it implies.

use ply_vm_proto::SpecDisk;

/// `/etc/hosts` for this instance: loopback, its own name, then the peers
/// the parent resolved.
///
/// The same *names* an instance resolves on Linux, not the same file. The
/// namespace backend binds a copy of the HOST's `/etc/hosts` with the
/// instance's lines appended (`runtime/hosts.rs::compose_from`), so a
/// developer's own entries come along; this is synthesized from the spec
/// disk and carries only ply's four parts. No app can tell the difference —
/// every name still resolves to the same address — but do not read this as
/// byte parity, because it is not.
pub fn hosts_file(spec: &SpecDisk) -> String {
    let mut out = String::from("127.0.0.1\tlocalhost\n");
    // An app that resolves its own hostname must get an answer; the Linux
    // backend gets this from the host's /etc/hosts bind, whose instance-local
    // half (`ns/container.rs`) is this same `127.0.0.1\t<hostname>` line.
    // Without it, anything calling getfqdn() at bind stalls in DNS instead.
    out.push_str(&format!("127.0.0.1\t{}\n", spec.hostname));
    out.push_str("::1\tlocalhost ip6-localhost ip6-loopback\n");
    for (ip, name) in &spec.hosts {
        out.push_str(&format!("{ip}\t{name}\n"));
    }
    out
}

/// `/etc/resolv.conf`, or `None` when the run has no resolver — a standalone
/// `ply run` with no switch.
///
/// `None` leaves the file alone, and the reason is NOT "the guest keeps
/// whatever the image shipped", which is what this said and what the plan
/// flagged as wrong: ply images are per-package squashfs layers, and neither
/// `alpine-baselayout` nor Debian `base-files` ships an `/etc/resolv.conf`,
/// so in practice there is nothing to keep and the guest ends up with no
/// resolver at all. That is the correct outcome — there is no resolver —
/// but a reader who believes the old reason will act on it.
///
/// Two known gaps with the namespace backend, recorded on the function they
/// belong to: `ns/container.rs` always writes the file (both arms), and
/// `resolv_conf_via` preserves the host's `search` and `options` lines, which
/// this cannot because `SpecDisk` has no `dns_search` field yet.
pub fn resolv_conf(spec: &SpecDisk) -> Option<String> {
    spec.dns.as_ref().map(|ip| format!("nameserver {ip}\n"))
}

/// Which attached device is the spec disk. Every disk is scanned for the
/// `PLYSPEC1` magic rather than a position being trusted (ruling R0-5):
/// same contract, one failure mode fewer.
///
/// `read_head` returns the leading bytes of one device, or `None` when that
/// device does not exist. The scan stops at the first absent device because
/// virtio disks are named contiguously from `vda`: a gap means there is
/// nothing beyond it, and probing on would only cost open() calls that all
/// fail.
///
/// The returned bytes are whatever `read_head` handed back for that device,
/// which the caller then feeds to `ply_vm_proto::decode_spec_disk`. A
/// `SpecError::Truncated` from those bytes means "this IS the spec disk and
/// the caller did not read enough of it (or it is damaged)" — never "keep
/// scanning": `is_spec_disk` and `decode_spec_disk` share the invariant that
/// anything the scan accepts never comes back as `SpecError::Magic`.
pub fn find_spec_disk<F>(read_head: F, max_devices: usize) -> Option<(usize, Vec<u8>)>
where
    F: Fn(&str) -> Option<Vec<u8>>,
{
    for index in 0..max_devices {
        let dev = crate::overlay::device_name(index);
        // Devices are contiguous from vda; the first absent one is the end.
        let head = read_head(&dev)?;
        if ply_vm_proto::is_spec_disk(&head) {
            return Some((index, head));
        }
    }
    None
}

/// Resolve `argv[0]` against the COMPOSED `PATH` from the spec disk.
///
/// The same job `ns/container.rs::resolve_program` does on Linux and for the
/// same reason: a manifest's entrypoint is routinely a bare name (`postgres`,
/// `redis-server`), and `execve` — unlike `execvpe` — searches nothing. Doing
/// it here rather than reaching for `execvpe` is deliberate on both backends:
/// `execvpe` searches the *caller's* `PATH`, which inside the guest is PID
/// 1's, not the app's composed one.
///
/// `is_exec` answers "is this path an executable file"; it is a parameter so
/// the ordering rule is testable without a filesystem. A name containing `/`
/// is used as-is, exactly as `execve` would treat it.
pub fn resolve_program(name: &str, path: &str, is_exec: impl Fn(&str) -> bool) -> String {
    if name.contains('/') {
        return name.to_string();
    }
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let candidate = format!("{}/{name}", dir.trim_end_matches('/'));
        if is_exec(&candidate) {
            return candidate;
        }
    }
    // Unresolved: hand `execve` the bare name so its ENOENT names the thing
    // the manifest actually asked for, rather than the last directory tried.
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn spec() -> SpecDisk {
        SpecDisk {
            entrypoint: vec!["/bin/true".into()],
            workdir: "/".into(),
            user: None,
            env: vec![],
            hostname: "db".into(),
            hosts: vec![("10.77.0.3".into(), "web.ply".into())],
            dns: Some("10.77.0.1".into()),
            net: None,
            volumes: vec![],
            params_seed: vec![],
            layer_count: 1,
        }
    }

    #[test]
    fn hosts_carries_loopback_the_instances_own_name_and_its_peers() {
        let text = hosts_file(&spec());
        assert!(text.contains("127.0.0.1\tlocalhost"));
        assert!(
            text.contains("127.0.0.1\tdb"),
            "an app resolving its own hostname must not fail"
        );
        assert!(text.contains("10.77.0.3\tweb.ply"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn no_resolver_in_the_spec_disk_writes_no_resolv_conf() {
        let mut s = spec();
        s.dns = None;
        assert_eq!(resolv_conf(&s), None);
        assert_eq!(
            resolv_conf(&spec()).as_deref(),
            Some("nameserver 10.77.0.1\n")
        );
    }
}

#[cfg(test)]
mod disk_tests {
    use super::*;
    use ply_vm_proto::{encode_spec_disk, SPEC_MAGIC};

    #[test]
    fn the_spec_disk_is_found_by_its_magic_wherever_it_sits() {
        let spec_bytes = encode_spec_disk(&super::tests::spec()).unwrap();
        let heads = move |dev: &str| match dev {
            "/dev/vda" => Some(b"hsqs....squashfs".to_vec()),
            "/dev/vdb" => Some(b"hsqs....squashfs".to_vec()),
            "/dev/vdc" => Some(spec_bytes.clone()),
            _ => None,
        };
        let (index, bytes) = find_spec_disk(heads, 8).expect("found");
        assert_eq!(index, 2);
        assert_eq!(&bytes[..8], SPEC_MAGIC);
    }

    #[test]
    fn no_spec_disk_at_all_is_a_none_not_a_panic() {
        assert!(find_spec_disk(|_| Some(b"hsqs".to_vec()), 4).is_none());
    }

    #[test]
    fn scanning_stops_at_the_first_absent_device() {
        let seen = std::cell::RefCell::new(Vec::new());
        let result = find_spec_disk(
            |dev| {
                seen.borrow_mut().push(dev.to_string());
                None
            },
            8,
        );
        assert!(result.is_none());
        assert_eq!(
            seen.borrow().len(),
            1,
            "a gap in the device list ends the scan"
        );
    }

    #[test]
    fn a_bare_entrypoint_name_is_resolved_against_the_composed_path() {
        // The manifest says `postgres`; the image puts it in /opt/db/bin.
        let path = "/opt/db/bin:/usr/bin:/bin";
        assert_eq!(
            resolve_program("postgres", path, |p| p == "/opt/db/bin/postgres"),
            "/opt/db/bin/postgres"
        );
        // First hit in PATH order wins, not last.
        assert_eq!(resolve_program("sh", path, |_| true), "/opt/db/bin/sh");
        // A path with a slash is execve's business, not ours.
        assert_eq!(resolve_program("/bin/true", path, |_| false), "/bin/true");
        assert_eq!(resolve_program("./run.sh", "", |_| true), "./run.sh");
        // Nothing found: hand execve the name the manifest asked for, so its
        // ENOENT names that and not the last directory tried.
        assert_eq!(resolve_program("nope", path, |_| false), "nope");
        // An empty PATH element is skipped, never turned into "/nope".
        assert_eq!(resolve_program("nope", "::", |_| true), "nope");
    }
}
