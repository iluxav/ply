//! The spec disk: one small read-only disk that carries this instance's
//! entrypoint, env, hostname, `/etc/hosts` and volume table to the guest
//! init, so none of it has to travel through argv or the kernel cmdline.
//!
//! Portable by construction — it is a byte format over `ply_vm_proto`'s
//! types and nothing else, so its tests run on Linux CI.
//!
//! # The disk order this module encodes is the guest's whole world
//!
//! `volumes[].dev` is a device PATH, and the guest mounts whatever is at
//! that path without a second opinion — [`volume_devs`] is what decides
//! those paths, and Task 8's device tree is what has to make them true.
//! The contract, in one place, is on [`volume_devs`].

use std::net::Ipv4Addr;
use std::path::Path;

use ply_vm_proto::{encode_spec_disk, NetSpec, ParamsTree, SpecDisk, UserSpec, VolumeSpec};

use crate::error::{Error, Result};
use crate::runtime::backend::InstanceSpec;
use crate::runtime::params_tree;
use crate::runtime::vm::switch;

/// The largest image the guest init will read back off the device.
///
/// A second copy of `ply-guest-init`'s `SPEC_READ_CAP`, and the reason the
/// duplication is worth its risk: the guest `fail()`s on a longer disk, and
/// it does so from inside a VM, on a console line, after the host has
/// already claimed ports and written state. [`write`] refuses the same disk
/// on the host, where the message reaches a person. The two constants must
/// move together; if a third consumer ever needs it, its home is
/// `ply-vm-proto` beside `SECTOR`, which both sides already link.
pub const GUEST_READ_CAP: usize = 4 * 1024 * 1024;

/// `/dev/vda`, `/dev/vdb`, … `/dev/vdz`, `/dev/vdaa`: Linux's own
/// `sd_format_disk_name` scheme, base-26 with no zero digit.
///
/// This is a second copy of `ply-guest-init`'s `overlay::device_name`, which
/// is the function that actually resolves these paths inside the guest.
/// `ply-core` cannot call it — the guest init is a binary crate, and it is
/// built for a different target — and the honest fix is to hoist it into
/// `ply-vm-proto`, exactly as `PARENT_OWNED` was hoisted. Until then both
/// copies are pinned by the same five external facts (`vda`, `vdb`, `vdz`,
/// `vdaa`, `vdab`), spelled out in a test on each side.
pub fn device_name(index: usize) -> String {
    let mut suffix = Vec::new();
    let mut n = index as i64;
    while n >= 0 {
        suffix.push(b'a' + (n % 26) as u8);
        n = n / 26 - 1;
    }
    suffix.reverse();
    // `suffix` is ASCII by construction, so this cannot fail; say so without
    // an `expect`, which would be a panic in a supervisor.
    format!("/dev/vd{}", String::from_utf8_lossy(&suffix))
}

/// The device path each volume will have inside the guest, in
/// `InstanceSpec.binds` order.
///
/// # This is a contract with Task 8's device tree, and nothing else enforces it
///
/// The guest reads `volumes[].dev` and mounts it. It does NOT probe for the
/// volume the way it probes for the spec disk (a magic scan, ruling R0-5)
/// or for the layers (the squashfs magic at each of `0..layer_count`),
/// because a volume is the one disk whose content is arbitrary — a
/// first-boot volume is all zeroes and looks like nothing at all. So the
/// name here is trusted, and it is only true if the VMM attaches disks in
/// this order:
///
/// ```text
///   0 .. layer_count            the image layers, overlay order, top first
///   layer_count .. + binds      one writable disk per volume, in binds order
///   after those                 the spec disk (found by magic, so its exact
///                               position does not matter — but it must not
///                               come BEFORE a volume, or every volume shifts)
/// ```
///
/// Task 2 proved this is not theoretical: qemu's `virt` machine hands out
/// virtio-mmio transports in **reverse**, so the layer passed first came up
/// as `/dev/vdb`. Task 8 emits ply's own device tree and owns the mapping;
/// this function is the half of it the guest is told about. Get them out of
/// step and there is no error — a database mounts an empty disk, or the
/// wrong one.
pub fn volume_devs(layer_count: usize, volumes: usize) -> Vec<String> {
    (0..volumes).map(|i| device_name(layer_count + i)).collect()
}

/// `/etc/hosts` lines for the siblings this instance must be able to name.
///
/// `InstanceSpec.local_aliases` is the list of names (`--netns-peer`, one
/// per sibling, never this instance itself); `peers` is where the backend
/// has actually placed them on the switch.
///
/// # Why these are NOT `127.0.0.1`, though the Linux backend's are
///
/// `ns/container.rs` writes `127.0.0.1\t<alias>.ply` for exactly this list,
/// and it is right there: rootless stack members share ONE network
/// namespace, so a sibling really is reachable on loopback. A microVM
/// shares nothing — each member is its own machine with its own address on
/// the switch. A loopback line here would not merely fail to help, it would
/// point every cross-member connection back into the caller's own guest, and
/// it would keep doing so even though the switch's own resolver answers
/// `<name>.ply` correctly, because `/etc/hosts` is consulted first and wins.
///
/// That is also why an alias with no address is dropped rather than given a
/// placeholder: no line at all leaves the switch's resolver free to answer,
/// which is the outcome that eventually becomes correct on its own.
pub fn hosts_lines(spec: &InstanceSpec, peers: &[(String, Ipv4Addr)]) -> Vec<(String, String)> {
    spec.local_aliases
        .iter()
        .filter_map(|alias| {
            let (_, ip) = peers.iter().find(|(name, _)| name == alias)?;
            Some((ip.to_string(), format!("{alias}.ply")))
        })
        .collect()
}

/// A snapshot of the host's params tree for `app` and each of `peers`, to
/// seed `/run/ply` inside the guest.
///
/// The namespace backend bind-mounts `run_dir()/params` into the container,
/// so a container reads the tree LIVE and sees a peer's `state` change under
/// it. A guest cannot: it gets this copy, taken at launch. That difference
/// is the same one the plan already records for `/etc/hosts` — the control
/// channel's `{"params":…}` line (Task 9) is what refreshes it afterwards,
/// and `--after` gates are evaluated on the host, where the tree is live, so
/// nothing depends on this snapshot being current.
///
/// Deterministic on purpose: apps in the order given, keys sorted, so two
/// runs of the same stack produce byte-identical spec disks. Best effort in
/// every other way — an app with no directory yet contributes no node (the
/// guest would only create an empty one), and a fact that is not UTF-8, or
/// is not a plain file, is skipped rather than failing a launch.
pub fn params_seed(app: &str, peers: &[String]) -> ParamsTree {
    let mut out = ParamsTree::new();
    let mut seen: Vec<&str> = Vec::new();
    for name in std::iter::once(app).chain(peers.iter().map(String::as_str)) {
        if name.is_empty() || seen.contains(&name) {
            continue;
        }
        seen.push(name);
        let Ok(entries) = std::fs::read_dir(params_tree::dir(name)) else {
            continue; // never launched, or nothing published yet
        };
        let mut facts: Vec<(String, String)> = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let Ok(key) = entry.file_name().into_string() else {
                continue; // not UTF-8: it cannot have been published by ply
            };
            // `read` trims, exactly as every other reader of this tree does,
            // so a value crosses the machine boundary as the same string
            // `params_tree::read` would have returned on the host.
            if let Some(value) = params_tree::read(name, &key) {
                facts.push((key, value));
            }
        }
        facts.sort();
        out.push((name.to_string(), facts));
    }
    out
}

/// Build the guest's view of this instance.
///
/// `volume_devs` is the device path for each of `spec.binds`, in the same
/// order — normally [`volume_devs`]'s output, and an empty slice is the safe
/// spelling of "use the contract's own names". A caller that hands over
/// FEWER names than there are binds does not lose a volume: the rest are
/// named from the contract too, because a dropped volume is an app starting
/// on an empty directory where its database should be, with no error
/// anywhere.
///
/// `dns` is the resolver the backend itself knows about (the switch's).
/// `None` falls back to `spec.dns`, the one the supervisor was given on
/// `--netns-dns`: the two are the same resolver in every path that exists
/// today, and a caller that passes `None` must not thereby leave a stack
/// member unable to resolve anything.
///
/// `peers` is `(sibling name, its address on the switch)`; see
/// [`hosts_lines`] for why an address is required and loopback is not it.
///
/// `address` is this instance's own address on the switch, which the guest
/// configures `eth0` with. `None` is a guest with no network card — what
/// every instance had before the switch existed, and what one still gets if
/// the parent could not start a switch. The prefix and the gateway are NOT
/// parameters: they are the switch's own constants, read from
/// [`switch::PREFIX_LEN`] and [`switch::GATEWAY`] here, so the address the
/// guest is told to use and the network it is told to use cannot disagree.
pub fn build(
    spec: &InstanceSpec,
    volume_devs: &[String],
    dns: Option<String>,
    peers: &[(String, Ipv4Addr)],
    address: Option<Ipv4Addr>,
) -> SpecDisk {
    let layer_count = spec.images.len();
    SpecDisk {
        entrypoint: spec.entrypoint.clone(),
        workdir: spec.cwd.to_string_lossy().into_owned(),
        user: spec.run_user.as_ref().map(|u| UserSpec {
            name: u.name.clone(),
            uid: u.uid,
            gid: u.gid,
        }),
        env: spec.env.clone(),
        hostname: spec.hostname.clone(),
        hosts: hosts_lines(spec, peers),
        dns: dns.or_else(|| spec.dns.clone()),
        net: address.map(|ip| NetSpec {
            ip: ip.to_string(),
            prefix_len: switch::PREFIX_LEN,
            gateway: switch::GATEWAY.to_string(),
        }),
        volumes: spec
            .binds
            .iter()
            .enumerate()
            .map(|(i, (_, path))| VolumeSpec {
                path: path.clone(),
                dev: volume_devs
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| device_name(layer_count + i)),
            })
            .collect(),
        // Keyed by `hostname`, because that is the name the guest matches
        // its own node by when it seeds `/run/ply/self`; `run.rs` sets
        // `hostname = app` and publishes under the same name.
        params_seed: params_seed(&spec.hostname, &spec.local_aliases),
        // `images` and nothing else: the guest refuses to boot unless each of
        // devices `0..layer_count` carries the squashfs magic.
        layer_count,
    }
}

/// Write it, readable only by this user: the composed env in here includes
/// every secret the run resolved.
///
/// The mode is set twice on purpose. `OpenOptions::mode` applies only when
/// the file is CREATED, so a stale `spec.img` left by a killed instance —
/// the instance directory outlives its parent until `reap_stale` scrubs it
/// — would keep whatever mode it had and publish every secret to the whole
/// machine. `set_permissions` after the open makes 0600 true either way, and
/// it happens BEFORE the bytes are written.
pub fn write(path: &Path, disk: &SpecDisk) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let bytes = encode_spec_disk(disk)
        .map_err(|e| Error::Runtime(format!("building the spec disk: {e}")))?;
    // Before anything is created: a disk the guest cannot read back is a VM
    // that boots, prints one line to a console nobody is reading, and powers
    // off. Say it here instead, and leave no half-written image behind.
    if bytes.len() > GUEST_READ_CAP {
        return Err(Error::Runtime(format!(
            "the spec disk for this instance is {} bytes and the guest init reads at most \
             {GUEST_READ_CAP} — the composed environment or the params tree is too large \
             to hand to a microVM",
            bytes.len()
        )));
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| Error::Io {
            path: path.into(),
            source,
        })?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|source| Error::Io {
            path: path.into(),
            source,
        })?;
    file.write_all(&bytes).map_err(|source| Error::Io {
        path: path.into(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn instance_spec(images: usize) -> InstanceSpec {
        InstanceSpec {
            app: "db".into(),
            package: "postgres".into(),
            n: 0,
            instance_dir: PathBuf::from("/nonexistent/instances/db.0"),
            images: (0..images)
                .map(|i| PathBuf::from(format!("/store/layer-{i}.img")))
                .collect(),
            entrypoint: vec!["/opt/postgres/bin/postgres".into()],
            cwd: PathBuf::from("/opt/postgres"),
            env: Vec::new(),
            hostname: "db".into(),
            binds: Vec::new(),
            volume_targets: Vec::new(),
            run_user: None,
            capabilities: None,
            keep_net_bind: false,
            privileged: false,
            resources: None,
            dns: None,
            local_aliases: Vec::new(),
        }
    }

    fn instance_spec_with_env(env: Vec<(String, String)>) -> InstanceSpec {
        InstanceSpec {
            env,
            ..instance_spec(2)
        }
    }

    fn instance_spec_with_volumes(paths: Vec<String>) -> InstanceSpec {
        InstanceSpec {
            binds: paths
                .iter()
                .map(|p| {
                    (
                        PathBuf::from(format!("/var/lib/ply/volumes/db{p}")),
                        p.clone(),
                    )
                })
                .collect(),
            volume_targets: paths,
            ..instance_spec(2)
        }
    }

    #[test]
    fn the_disk_carries_the_env_the_supervisor_composed_not_the_manifests() {
        let spec = instance_spec_with_env(vec![("POSTGRES_PASSWORD".into(), "s3cret".into())]);
        let disk = build(&spec, &[], None, &[], None);
        assert_eq!(disk.env, spec.env, "env crosses in the spec disk, verbatim");
    }

    #[test]
    fn volumes_are_named_with_the_device_that_carries_them() {
        let spec = instance_spec_with_volumes(vec!["/var/lib/pg".into(), "/var/log/pg".into()]);
        // Two image layers, so volumes start at vdc.
        let disk = build(
            &spec,
            &["/dev/vdc".into(), "/dev/vdd".into()],
            None,
            &[],
            None,
        );
        assert_eq!(disk.volumes[0].path, "/var/lib/pg");
        assert_eq!(disk.volumes[0].dev, "/dev/vdc");
        assert_eq!(disk.volumes[1].dev, "/dev/vdd");
    }

    #[test]
    fn a_secret_never_reaches_a_file_anyone_else_can_read() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("spec.img");
        write(
            &path,
            &build(
                &instance_spec_with_env(vec![("K".into(), "v".into())]),
                &[],
                None,
                &[],
                None,
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the spec disk holds composed secrets");
    }

    #[test]
    fn the_written_image_is_what_the_guest_will_decode() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("spec.img");
        let disk = build(
            &instance_spec_with_env(vec![]),
            &[],
            Some("10.77.0.1".into()),
            &[],
            Some(Ipv4Addr::new(10, 77, 0, 2)),
        );
        write(&path, &disk).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(ply_vm_proto::decode_spec_disk(&bytes).unwrap(), disk);
    }

    // --- the four tests above are the plan's; everything below is Task 7's -

    #[test]
    fn rewriting_a_spec_disk_over_a_readable_file_still_leaves_it_unreadable() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("spec.img");
        // A file already there with someone else's mode: `OpenOptions::mode`
        // applies on CREATE only, so an existing 0644 would survive it and
        // publish every composed secret to the whole machine.
        std::fs::write(&path, b"stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write(&path, &build(&instance_spec(1), &[], None, &[], None)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn the_layer_count_is_the_number_of_images_and_nothing_else() {
        // The guest refuses to boot unless every one of these devices carries
        // the squashfs magic, so this may only ever come from `images`.
        assert_eq!(
            build(&instance_spec(1), &[], None, &[], None).layer_count,
            1
        );
        assert_eq!(
            build(&instance_spec(5), &[], None, &[], None).layer_count,
            5
        );
    }

    #[test]
    fn disks_are_named_the_way_linux_names_them() {
        // Byte-identical to `ply-guest-init`'s `overlay::device_name`, which
        // is what the guest resolves these paths with.
        assert_eq!(device_name(0), "/dev/vda");
        assert_eq!(device_name(1), "/dev/vdb");
        assert_eq!(device_name(25), "/dev/vdz");
        assert_eq!(device_name(26), "/dev/vdaa");
        assert_eq!(device_name(27), "/dev/vdab");
    }

    #[test]
    fn the_volume_devices_are_the_ones_after_the_last_image_layer() {
        // The plan's own test above hardcodes vdc/vdd for two layers; this is
        // the function that has to produce them, so Task 8 never has to
        // count letters by hand.
        assert_eq!(volume_devs(2, 2), ["/dev/vdc", "/dev/vdd"]);
        assert_eq!(volume_devs(1, 1), ["/dev/vdb"]);
        assert!(volume_devs(3, 0).is_empty());
    }

    #[test]
    fn a_volume_the_caller_named_no_device_for_is_never_silently_dropped() {
        // `zip` would have dropped it: two binds, one device, one volume in
        // the disk, and the app starts with an empty directory where its
        // database should be. Every bind gets a device, and the ones the
        // caller did not name get the contract's own.
        let spec = instance_spec_with_volumes(vec!["/var/lib/pg".into(), "/var/log/pg".into()]);
        let disk = build(&spec, &["/dev/vdc".into()], None, &[], None);
        assert_eq!(disk.volumes.len(), 2);
        assert_eq!(disk.volumes[1].dev, "/dev/vdd");
        // And a caller that names none at all still gets a correct table.
        let disk = build(&spec, &[], None, &[], None);
        assert_eq!(
            disk.volumes
                .iter()
                .map(|v| v.dev.as_str())
                .collect::<Vec<_>>(),
            ["/dev/vdc", "/dev/vdd"]
        );
    }

    #[test]
    fn a_stack_members_siblings_resolve_to_their_own_addresses_not_to_loopback() {
        // `InstanceSpec.local_aliases` is the list of names that must resolve
        // inside this instance. On Linux the siblings share one namespace and
        // ARE loopback; here each one is its own machine on the switch, so a
        // `127.0.0.1` line would point the app at itself.
        let spec = InstanceSpec {
            local_aliases: vec!["web".into(), "cache".into()],
            ..instance_spec(2)
        };
        let peers = [
            ("web".to_string(), Ipv4Addr::new(10, 77, 0, 3)),
            ("cache".to_string(), Ipv4Addr::new(10, 77, 0, 4)),
        ];
        let disk = build(&spec, &[], None, &peers, None);
        assert_eq!(
            disk.hosts,
            vec![
                ("10.77.0.3".to_string(), "web.ply".to_string()),
                ("10.77.0.4".to_string(), "cache.ply".to_string()),
            ]
        );
        assert!(
            !disk.hosts.iter().any(|(ip, _)| ip == "127.0.0.1"),
            "a sibling at loopback is this guest itself"
        );
    }

    #[test]
    fn an_alias_with_no_address_yet_is_left_to_dns_rather_than_pointed_somewhere_wrong() {
        // /etc/hosts SHADOWS the switch's DNS: a placeholder line here would
        // beat the right answer for the whole life of the instance, so an
        // alias the caller could not place gets no line at all.
        let spec = InstanceSpec {
            local_aliases: vec!["web".into(), "cache".into()],
            ..instance_spec(2)
        };
        let peers = [("web".to_string(), Ipv4Addr::new(10, 77, 0, 3))];
        let disk = build(&spec, &[], None, &peers, None);
        assert_eq!(disk.hosts, vec![("10.77.0.3".into(), "web.ply".into())]);
    }

    #[test]
    fn the_resolver_the_supervisor_already_has_is_not_lost_when_the_caller_names_none() {
        let with_dns = InstanceSpec {
            dns: Some("10.77.0.1".into()),
            ..instance_spec(1)
        };
        assert_eq!(
            build(&with_dns, &[], None, &[], None).dns.as_deref(),
            Some("10.77.0.1"),
            "`--netns-dns` reached the backend through InstanceSpec; dropping it \
             leaves the guest with no resolver at all"
        );
        // The backend's own switch resolver wins when it has one: it is the
        // later fact of the two.
        assert_eq!(
            build(&with_dns, &[], Some("10.77.0.9".into()), &[], None)
                .dns
                .as_deref(),
            Some("10.77.0.9")
        );
    }

    /// The guest gets its address from the disk, and it gets the switch's
    /// own prefix and gateway with it — the three cannot be set apart, which
    /// is what stops an instance being told an address on a network it is
    /// not on.
    #[test]
    fn the_address_on_the_disk_is_the_switchs_own_network() {
        let disk = build(
            &instance_spec(1),
            &[],
            None,
            &[],
            Some(Ipv4Addr::new(10, 77, 0, 2)),
        );
        let net = disk.net.expect("an instance on the switch has an address");
        assert_eq!(net.ip, "10.77.0.2");
        assert_eq!(net.prefix_len, switch::PREFIX_LEN);
        assert_eq!(net.gateway, switch::GATEWAY.to_string());
        // No switch, no card: the guest must not be handed an address on a
        // network nothing is serving.
        assert_eq!(build(&instance_spec(1), &[], None, &[], None).net, None);
    }

    #[test]
    fn the_params_seed_carries_this_instances_own_facts_and_its_peers() {
        let _guard = crate::paths::ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os("XDG_RUNTIME_DIR");
        let td = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", td.path());

        params_tree::publish("db", "state", "starting").unwrap();
        params_tree::publish("db", "instances", "1").unwrap();
        params_tree::publish("web", "state", "healthy").unwrap();

        let spec = InstanceSpec {
            local_aliases: vec!["web".into(), "never-launched".into()],
            ..instance_spec(1)
        };
        let disk = build(&spec, &[], None, &[], None);

        // Own node first, keyed by the name the guest matches it with.
        assert_eq!(disk.params_seed[0].0, "db");
        assert_eq!(
            disk.params_seed[0].1,
            vec![
                ("instances".to_string(), "1".to_string()),
                ("state".to_string(), "starting".to_string()),
            ],
            "keys in a fixed order: two runs of one stack write identical disks"
        );
        assert_eq!(
            disk.params_seed[1],
            ("web".into(), vec![("state".into(), "healthy".into())])
        );
        assert_eq!(
            disk.params_seed.len(),
            2,
            "a peer that has published nothing has no node to seed"
        );

        match previous {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn a_spec_disk_the_guest_could_not_read_is_refused_here_instead() {
        // The guest reads at most 4 MiB back off the device and `fail()`s on
        // anything longer — inside a VM, where the only evidence is a console
        // line. Refuse it on the host, where the message reaches a person.
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("spec.img");
        let spec = instance_spec_with_env(vec![("BLOB".into(), "x".repeat(GUEST_READ_CAP))]);
        let err = write(&path, &build(&spec, &[], None, &[], None)).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("4194304"),
            "the message must name the limit: {message}"
        );
        assert!(
            !path.exists(),
            "a refused disk must not leave a half-written image the VMM would attach"
        );
    }
}
