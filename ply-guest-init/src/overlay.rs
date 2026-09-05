//! Turning the disks the VMM attached into the root filesystem, expressed
//! as pure functions so Linux CI tests them without a VM.

/// `/dev/vda`, `/dev/vdb`, … `/dev/vdz`, `/dev/vdaa`. Linux names virtio
/// disks in probe order, and the VMM's device tree fixes that order, so
/// index 0 is always the top image layer.
pub fn device_name(index: usize) -> String {
    // Linux's own scheme (`sd_format_disk_name` in drivers/scsi/sd.c, which
    // virtio-blk reuses): a..z, then aa..zz — base-26 with NO zero digit, so
    // index 26 is `vdaa` and not `vdba`. A plain base-26 conversion gets that
    // one wrong, which is why the `- 1` below is load-bearing: it borrows the
    // digit that a zero-less system has no way to write.
    //
    // Done in `usize` with an explicit break rather than the kernel's
    // `while (n >= 0)` over a signed counter: an `as i64` cast would make
    // `usize::MAX` come out as -1, skip the loop entirely, and return the
    // bare `/dev/vd` — a wrong device path with no panic and no error, in a
    // PID 1 that aborts rather than unwinds. Pushing `char`s also removes a
    // `String::from_utf8(..).expect()` whose safety had to be argued from
    // the arithmetic instead of being structural.
    let mut suffix = String::new();
    let mut n = index;
    loop {
        suffix.push((b'a' + (n % 26) as u8) as char);
        match n / 26 {
            0 => break,
            next => n = next - 1,
        }
    }
    format!("/dev/vd{}", suffix.chars().rev().collect::<String>())
}

/// overlayfs `lowerdir=` for `count` layers mounted under `mounts/<i>`.
///
/// overlayfs takes lowers TOP FIRST, colon-separated — the same order
/// `InstanceSpec.images` uses and the same order `runtime/ns/mount.rs`
/// builds on Linux, so one lockfile means one filesystem on both backends.
pub fn lowerdir(mount_root: &str, count: usize) -> String {
    // Mount point `i` holds `InstanceSpec.images[i]`, and the VMM attaches
    // those disks in that same order, so joining 0..count top-first here is
    // exactly what `ns/mod.rs` does when it pushes its layers in `images`
    // order and hands them to `ns/mount.rs::mount_overlay`, which joins them
    // with `:` unchanged. The two backends agree by construction.
    let lowers: Vec<String> = (0..count).map(|i| format!("{mount_root}/{i}")).collect();
    format!("lowerdir={}", lowers.join(":"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disks_are_named_the_way_linux_names_them() {
        assert_eq!(device_name(0), "/dev/vda");
        assert_eq!(device_name(1), "/dev/vdb");
        assert_eq!(device_name(25), "/dev/vdz");
        assert_eq!(device_name(26), "/dev/vdaa");
        assert_eq!(device_name(27), "/dev/vdab");
        // The carries a two-level implementation would get wrong. Unreachable
        // today (the VMM caps devices at 32), but MAX_DEVICES lives in another
        // crate with no link to this test, and a bump to 64 would silently
        // uncover them.
        assert_eq!(device_name(51), "/dev/vdaz");
        assert_eq!(device_name(52), "/dev/vdba");
        assert_eq!(device_name(701), "/dev/vdzz");
        assert_eq!(device_name(702), "/dev/vdaaa");
    }

    #[test]
    fn the_overlay_stacks_the_app_image_on_top_of_its_packages() {
        // The same order the Linux backend uses: images[0] is the app.
        assert_eq!(lowerdir("/mnt", 3), "lowerdir=/mnt/0:/mnt/1:/mnt/2");
    }

    #[test]
    fn a_single_layer_still_makes_a_valid_lowerdir() {
        assert_eq!(lowerdir("/mnt", 1), "lowerdir=/mnt/0");
    }
}
