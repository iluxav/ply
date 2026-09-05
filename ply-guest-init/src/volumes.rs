//! Volume disks: format on first use, grow, mount, hand to the app's user.
//!
//! The host creates each volume as a sparse file and hands it over raw, so a
//! volume this guest has never booted on reads back as `HEAD_BYTES` zero
//! bytes and a volume it has booted on carries an ext4 superblock.
//! `classify` is the whole of the decision, and its rule is those two
//! sentences and nothing else: a disk that is neither is a disk this guest
//! does not understand, and formatting is not undoable.

/// Byte offset of `s_magic` from the start of the disk.
///
/// The ext2/3/4 superblock starts at byte 1024, and `s_magic` is at offset
/// **0x38 inside it** (`struct ext2_super_block`: four `__le32`s, then
/// twelve more fields, then the magic) — so 1080, not 1024.
///
/// This constant exists because getting it wrong is not a compile error and
/// not a test failure: it is a `classify` that answers "fresh disk" for
/// every ext4 volume ever made, and therefore an `mke2fs` over a live
/// database on its second boot. That is exactly what happened here, and
/// only a real boot caught it — the unit test agreed with the bug, because
/// the test wrote the magic wherever the implementation looked for it.
const EXT4_MAGIC_AT: usize = 1024 + 0x38;

/// How much of a disk's head every decision in this file is made from, and
/// the exact length `disk_head` promises to return or nothing.
///
/// It is a `const` shared by the reader and the rule because the two only
/// compose safely together: `classify` refuses a head shorter than this, so a
/// short read can never be mistaken for a blank disk.
///
/// # 68 KiB, and the number is load-bearing
///
/// The blank rule is "the head is all zeros", so how MUCH head it reads is
/// what decides which filesystems it can tell apart from a fresh sparse file
/// — and several leave their own first kilobytes deliberately empty:
///
/// * **btrfs** writes nothing in the leading 64 KiB and puts its first
///   superblock at `BTRFS_SUPER_INFO_OFFSET` = 0x10000, whose magic
///   `_BHRfS_M` sits at **0x10040**. Under the previous 4 KiB window a real
///   btrfs filesystem read as `Blank`, and `Blank` is the one answer that
///   leads to `mke2fs`.
/// * **ZFS** has the same shape: its L0 vdev label starts with an 8 KiB
///   blank pad and an 8 KiB boot header, so its first non-zero bytes (the
///   nvlist) are at 0x4000.
///
/// 68 KiB clears 0x10048 — the end of btrfs's magic — with room to spare, is
/// still one read of one buffer (~17 pages, once per volume per boot, next
/// to an `mke2fs` or a `mount`), and is still all zeros for the sparse file
/// the host actually creates. What it is NOT is a claim that every
/// filesystem is now covered; `classify` says what is and is not.
///
/// The cost of widening it: a device SHORTER than this now reads as nothing
/// at all (`disk_head` is exact), so it is refused with "the device is
/// missing or too short" rather than by name. Every real volume is orders of
/// magnitude larger — ext4's own minimum is far above 68 KiB — so the only
/// disks in that range are foreign ones that arrived by a host bug, and the
/// refusal is right either way.
pub const HEAD_BYTES: usize = 68 * 1024;

/// What the disk in a volume slot actually is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disk {
    /// `HEAD_BYTES` zero bytes: the host created this volume as a sparse
    /// file and nothing has ever written to it. **The only disposition that
    /// leads to `mke2fs`.**
    Blank,
    /// An ext4 superblock where `mke2fs` puts one. Mount it; never format it.
    Ext4,
    /// A squashfs image layer or the spec disk — the disk order shifted, and
    /// this device is somebody else's.
    Foreign,
    /// Anything else at all: an XFS or btrfs superblock, a partition table, a
    /// LUKS header, a volume written by a future ply, a file the host handed
    /// over by mistake, or an ext4 whose creation was interrupted before the
    /// superblock landed. This guest does not know what it is and therefore
    /// must not touch it.
    Unrecognised,
}

/// What is on this disk? The one decision that stands between a volume and
/// `mke2fs`, so it is stated as the host's invariant rather than as a list of
/// signatures to avoid.
///
/// The rule is **fail-closed**: `Blank` — and only `Blank` — is formatted.
/// The version this replaced asked the opposite question ("is there an ext4
/// magic at 1080?") and answered "format it" for everything else, which meant
/// every filesystem, header and partition table nobody had thought to
/// enumerate was silently destroyed on first boot. Inverting it costs nothing
/// and is cheaper to compute.
///
/// # What the rule covers, and the one thing it does not
///
/// To REFUSE, it needs no enumeration at all: a byte set anywhere in the
/// first `HEAD_BYTES` makes a disk `Unrecognised` whether or not this file
/// has ever heard of what wrote it. That is the whole of its strength, and
/// no signature list can match it.
///
/// Its weakness is the mirror image of that, and **btrfs is the family that
/// shows it rather than the one that settles it** — an earlier version of
/// this comment had that exactly backwards. A filesystem whose own head is
/// all zeros is indistinguishable from a fresh sparse file however carefully
/// the rule is phrased, and btrfs (magic at 0x10040) and ZFS (nvlist at
/// 0x4000) defeat BOTH shapes of check: a 4 KiB signature scan cannot see
/// their magic, and a 4 KiB zero-check cannot see them at all. The answer is
/// not a longer signature list, it is a window wide enough to reach them —
/// which is what `HEAD_BYTES` is for, and why its value is 68 KiB rather
/// than a page.
///
/// So the honest statement of the residual gap: a filesystem that leaves
/// `HEAD_BYTES` of zeros and writes its first byte past them would read as
/// `Blank` and be formatted. Nothing here would catch it. That is
/// unreachable through any supported flow — the host creates every backing
/// file itself, sparse, and only this guest ever writes one — and if some
/// future flow ever hands this guest a disk from elsewhere, **the window is
/// the thing that has to grow.**
///
/// `Foreign` is checked BEFORE the ext4 magic, and the order is load-bearing
/// in exactly one direction: the compressed body of a squashfs layer carries
/// `0x53 0xEF` at offset 1080 once every 65536 layers by chance, whereas an
/// ext4 volume whose first four bytes are `hsqs` needs a deliberately
/// installed boot block.
///
/// Neither misreading is destructive — `mke2fs` is reached only from
/// `Blank`, and neither a layer nor a volume is ever blank — so what the
/// order buys is the REMEDY the user is handed. A layer read as `Ext4` is
/// the misleading-remedy mistake: it goes to `mount_at`, fails, and comes
/// back as `unmountable(fresh: false)`, whose message tells the user to
/// delete the backing file of what is in fact an image layer. A volume read
/// as a layer refuses the boot and names the host's device order as the
/// thing to look at, which is where the bug actually is.
pub fn classify(head: &[u8]) -> Disk {
    // A short head is `Unrecognised`, not `Blank`: `disk_head` promises
    // never to produce one (see its own doc), and if that promise is ever
    // broken the answer must be the one that touches nothing.
    if head.len() < HEAD_BYTES {
        return Disk::Unrecognised;
    }
    // The window, not the whole slice: the rule is "the first HEAD_BYTES
    // are zero", and a caller that hands over more must not be able to widen
    // or narrow the decision by how much it read.
    if head[..HEAD_BYTES].iter().all(|b| *b == 0) {
        return Disk::Blank;
    }
    if is_foreign(head) {
        return Disk::Foreign;
    }
    if u16::from_le_bytes([head[EXT4_MAGIC_AT], head[EXT4_MAGIC_AT + 1]]) == 0xEF53 {
        return Disk::Ext4;
    }
    Disk::Unrecognised
}

/// Why this guest will not touch this device, or `None` when it will.
///
/// A refusal is the end of the instance, so it has to carry the remedy with
/// it: there is no `e2fsck` and no shell in this initramfs, so nothing inside
/// the guest can repair a volume, and a message that only says "refusing"
/// leaves a user with a VM that will not boot and no next step.
pub fn refusal(path: &str, dev: &str, disk: Disk) -> Option<String> {
    match disk {
        Disk::Blank | Disk::Ext4 => None,
        Disk::Foreign => Some(format!(
            "volume {path} names {dev}, but that device carries an image layer or the spec disk — \
             refusing to format it. The VMM's disk order and Linux's virtio-blk probe order \
             disagree; this is a bug in the host's device model, not in the volume, so do not \
             remove anything."
        )),
        Disk::Unrecognised => Some(format!(
            "volume {path} names {dev}, and that device is neither blank nor an ext4 volume — \
             refusing to format it, because whatever is on it would be destroyed. A volume this \
             guest created is ext4; a volume the host just created is all zeros. If this disk was \
             half-formatted by a VM that was killed, remove its backing file on the host and the \
             next start will make a fresh one (this loses whatever was on it, and there is no \
             e2fsck in this initramfs to try anything gentler)."
        )),
    }
}

/// The message for a volume that would not mount, which gets a sentence of
/// its own because it is the one failure a user cannot act on without being
/// told how.
///
/// The `fresh` case and the returning case are genuinely different bugs and
/// naming the wrong one sends a reader the wrong way:
///
/// * **Not fresh** is the dangerous one. A VM killed part-way through
///   `mke2fs` leaves a superblock on a filesystem that was never finished, so
///   every later boot classifies the disk as `Ext4`, correctly declines to
///   format it, and then cannot mount it either. There is no `e2fsck` in this
///   initramfs, so nothing inside the guest can repair it and the instance
///   never boots again — until somebody on the host removes the file.
/// * **Fresh** means `mke2fs` reported success on this very boot and the
///   kernel still refused the result, which is not about the volume's
///   contents at all.
pub fn unmountable(path: &str, dev: &str, err: &str, fresh: bool) -> String {
    if fresh {
        return format!(
            "volume {path}: {err} — {dev} was formatted successfully moments ago and the kernel \
             still will not mount it. That is not about anything stored on the volume: suspect \
             the guest kernel's ext4 support or the feature set in MKE2FS_ARGS, not the disk."
        );
    }
    format!(
        "volume {path}: {err} — {dev} carries an ext4 superblock but will not mount, which is \
         what a volume whose mke2fs was interrupted looks like. There is no e2fsck in this \
         initramfs, so the only remedy is to remove this volume's backing file on the host and \
         let the next start format a fresh one (its contents are lost either way)."
    )
}

/// Does this disk carry a filesystem that is emphatically NOT a volume?
///
/// `mke2fs` on an image layer would destroy it, silently and permanently,
/// and the failure would look like "the app's binary is the wrong version".
/// The spec disk names each volume's device explicitly, but the mapping from
/// a VMM's device-tree order to Linux's `/dev/vdN` probe order is exactly the
/// premise ruling R0-5 refused to trust for the spec disk — so before
/// formatting anything, check that what is there is not a layer or the spec
/// disk that landed one slot over.
///
/// `classify` no longer *depends* on this to be safe — anything unenumerated
/// is `Unrecognised` and refused anyway — but a wrong disk order is a host
/// bug with a specific fix, and telling a user that is worth two comparisons.
pub fn is_foreign(head: &[u8]) -> bool {
    // squashfs, little-endian: image layers. Both signatures are at offset
    // 0, so this reads a head of any length and never indexes past it.
    let squashfs = head.len() >= 4 && &head[..4] == b"hsqs";
    squashfs || ply_vm_proto::is_spec_disk(head)
}

/// The `mke2fs` arguments a ply volume is formatted with, after the device.
///
/// Two things here are load-bearing and neither is `mke2fs`'s default:
///
/// - **`-b 4096`.** `mke2fs`'s compiled-in profile has an `fs_types` entry
///   `small` for filesystems under 512 MiB that sets `blocksize = 1024`, and
///   a volume's block size is fixed for its whole life — `resize2fs` grows a
///   filesystem but never changes it. A database volume that starts at 256
///   MiB and grows to 50 GB would keep 1 KiB blocks forever: four times the
///   metadata reads per page and a divergence from what the Linux backend's
///   volumes (plain host directories on the user's own filesystem) give.
/// - **an explicit `-O` set with `^quota`.** This kernel is built without
///   `CONFIG_QUOTA`, and `ext4` refuses to mount a filesystem carrying the
///   `quota` feature when it cannot enable quotas — a hard, total mount
///   failure inside a VM. The tarball's own profile does not enable it, but
///   distributions patch that profile and `-t ext4` reads whatever
///   `/etc/mke2fs.conf` says; stating the set makes the guest's volumes
///   independent of which `mke2fs.conf` happened to be around.
///
/// The rest of the list is the tarball profile's own ext4 set, spelled out
/// so a future e2fsprogs bump cannot quietly add a feature this kernel has
/// no code for.
///
/// # `-F` is absent, and it is NOT a guard — do not treat it as one
///
/// An earlier version of this comment claimed that omitting `-F` made
/// `mke2fs` refuse a device it could see a filesystem on, "a second and
/// completely independent guard". That is false in both directions and the
/// truth is worse.
///
/// Read in the source of the e2fsprogs the guest actually runs: **1.47.4**,
/// the version `scripts/build-microvm-kernel.sh` pins in `E2FSVER` and
/// verifies by sha256 (`fd5bf388…`). Line numbers are from that tarball, so
/// the next reader re-checks in seconds instead of re-deriving it — and
/// checks them against `E2FSVER` first, because a citation to a version
/// nobody opened is exactly the defect this block exists to prevent.
///
/// * `misc/mke2fs.c:2913-2915` sets `CHECK_FS_EXIST` only `if (isatty(0) &&
///   isatty(1) && !offset)`, and every existing-filesystem probe in
///   `lib/support/plausible.c` is separately gated on that same flag — blkid
///   at :233, libmagic at :255, the partition-table check at :277 — with a
///   fall-through `return 1` ("plausible, go ahead") at :282. The guest's
///   `run` hands its child `/dev/null` on stdin, so `isatty(0)` is false and
///   **`mke2fs` never looks for an existing filesystem at all**. No refusal,
///   no message.
/// * Were stdin a tty — which it was, before `run` was fixed, because PID 1
///   inherits `/dev/console` on fd 0 — `check_plausibility` would return 0
///   and `mke2fs` would call `proceed_question(proceed_delay)`
///   (`misc/mke2fs.c:2920-2921`). `proceed_delay` is `-1` to begin with
///   (`misc/mke2fs.c:113`) and is only ever set from the profile, which
///   defaults it to `0` (`misc/mke2fs.c:2147`) — and there is no
///   `/etc/mke2fs.conf` in this initramfs anyway. Both values take the same
///   branch: `misc/util.c:97-126` runs the `alarm` path only for
///   `delay > 0`, and otherwise prints `Proceed anyway? (y,N)` and does a
///   bare blocking `fgets` on stdin. That is not a guard either: it is a
///   question on a console nobody is attached to, PID 1 blocked in
///   `wait_for` behind it, no `{"ready":true}` for the host, and nothing
///   that times out.
///
/// So there is exactly ONE guard on the volume path and it is `classify`.
/// `-F` stays out because it is not needed and its presence would say
/// something untrue about what is checked, not because it protects anything.
///
/// One bonus property, belt-and-braces and not part of that argument: since
/// `run` puts fd 0 on `/dev/null`, even an e2fsprogs that somehow reached
/// `proceed_question` could not hang the guest — `fgets` returns NULL at EOF
/// and `misc/util.c:118-123` reads that as "no" and `exit(1)`s. A refusal is
/// survivable and legible; a hang inside a VM is neither. That holds without
/// the `isatty` gate existing at all, which is what makes the paragraph
/// above safe to be wrong about in a future e2fsprogs bump.
pub const MKE2FS_ARGS: &[&str] = &[
    "-q",
    "-t",
    "ext4",
    "-b",
    "4096",
    "-O",
    "has_journal,extent,huge_file,flex_bg,dir_nlink,extra_isize,sparse_super,large_file,filetype,resize_inode,dir_index,metadata_csum,64bit,^quota,^project",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Where btrfs really puts its first superblock, and its magic inside
    /// it: `BTRFS_SUPER_INFO_OFFSET` = 0x10000, `struct btrfs_super_block`'s
    /// `magic` at +0x40. External facts, written down separately from the
    /// code that has to reach them.
    const BTRFS_SUPERBLOCK_AT: usize = 0x10000;
    const BTRFS_MAGIC_AT: usize = BTRFS_SUPERBLOCK_AT + 0x40;

    /// The head of a REAL btrfs filesystem: `mkfs.btrfs` leaves the leading
    /// 64 KiB alone and writes `_BHRfS_M` at 0x10040.
    ///
    /// Sized from the btrfs constants and NOT from `HEAD_BYTES`, on purpose.
    /// This fixture is the external fact the window is measured against, so
    /// narrowing `HEAD_BYTES` has to make the assertions below fail — if the
    /// fixture resized itself to match the code, the pair would agree with
    /// each other and with nothing else, which is precisely how the ext4
    /// magic came to be read at the wrong offset.
    fn btrfs_head() -> Vec<u8> {
        let mut head = vec![0u8; BTRFS_MAGIC_AT + 4096];
        head[BTRFS_MAGIC_AT..BTRFS_MAGIC_AT + 8].copy_from_slice(b"_BHRfS_M");
        head
    }

    /// The first `HEAD_BYTES` of a disk `mke2fs` has been over.
    fn ext4_head() -> Vec<u8> {
        // Built the way mke2fs builds it — superblock at 1024, magic at 0x38
        // inside it — rather than wherever the implementation happens to
        // look, which is how the offset came to be wrong in the first place.
        let mut head = vec![0u8; HEAD_BYTES];
        head[1080] = 0x53;
        head[1081] = 0xEF;
        head
    }

    #[test]
    fn a_sparse_file_the_host_just_created_is_the_only_disk_that_gets_formatted() {
        // The host's invariant, and the whole basis of the rule: a fresh
        // volume is a sparse file, so its head is 4096 zero bytes.
        assert_eq!(classify(&vec![0u8; HEAD_BYTES]), Disk::Blank);
        // One byte set anywhere in the head is enough to disqualify it. This
        // is the direction that matters: `Blank` is the only answer that
        // leads to mke2fs, so everything else must fall out of it.
        // The offsets past 4095 are the ones the old window could not see:
        // a byte at 0x4000 (ZFS's nvlist) or 0x10040 (btrfs's magic) is a
        // disk with a filesystem on it, and `Blank` is the answer that would
        // reformat it.
        for at in [
            0usize,
            1,
            512,
            1079,
            1082,
            4095,
            4096,
            0x4000,
            BTRFS_MAGIC_AT,
            HEAD_BYTES - 1,
        ] {
            let mut head = vec![0u8; HEAD_BYTES];
            head[at] = 1;
            assert_ne!(
                classify(&head),
                Disk::Blank,
                "a byte set at {at} is not a fresh sparse file"
            );
        }
    }

    #[test]
    fn a_disk_that_already_holds_ext4_is_mounted_never_reformatted() {
        assert_eq!(classify(&ext4_head()), Disk::Ext4);
    }

    #[test]
    fn the_magic_is_read_where_mke2fs_writes_it_and_nowhere_else() {
        // The regression that a real boot found and every unit test missed:
        // reading `s_magic` at 1024 instead of 1080 makes every formatted
        // volume look fresh, so the SECOND boot runs mke2fs over the user's
        // data. Both halves are asserted, because only the pair pins the
        // offset — "the right place says formatted" alone is satisfied by
        // reading two bytes anywhere they happen to match.
        let head = ext4_head();
        assert_eq!(&head[1080..1082], &[0x53, 0xEF], "mke2fs puts s_magic here");
        assert_eq!(classify(&head), Disk::Ext4);
        let mut wrong_place = vec![0u8; HEAD_BYTES];
        wrong_place[1024] = 0x53;
        wrong_place[1025] = 0xEF;
        // Note what the answer is NOT: under the old fail-open rule this was
        // `format me`. It is now a refusal, because 0xEF53 at the START of
        // the superblock is not the magic and a disk carrying bytes we do not
        // understand is never a disk we may destroy.
        assert_eq!(classify(&wrong_place), Disk::Unrecognised);
    }

    #[test]
    fn a_layer_or_the_spec_disk_in_a_volume_slot_is_never_formatted() {
        // The disk-order hazard, made non-destructive: the smoke test found
        // qemu's `virt` handing out virtio-mmio transports in reverse, so a
        // volume device that is really a layer is a live possibility, not a
        // hypothetical.
        let mut layer = vec![0u8; HEAD_BYTES];
        layer[..4].copy_from_slice(b"hsqs");
        assert!(is_foreign(&layer));
        assert_eq!(classify(&layer), Disk::Foreign);
        let mut spec = vec![0u8; HEAD_BYTES];
        spec[..8].copy_from_slice(ply_vm_proto::SPEC_MAGIC);
        assert!(is_foreign(&spec));
        assert_eq!(classify(&spec), Disk::Foreign);
        // A blank volume and a formatted one are both perfectly at home.
        assert!(!is_foreign(&vec![0u8; HEAD_BYTES]));
        assert!(!is_foreign(&ext4_head()));
    }

    #[test]
    fn anything_that_is_neither_blank_nor_ext4_is_refused_rather_than_destroyed() {
        // The fail-open version of this decision answered "format me" for
        // every one of these, and formatting is not undoable. `is_foreign`
        // enumerates two signatures; this rule needs no enumeration at all,
        // which is exactly why it catches the ones nobody thought of.
        let cases: &[(&str, Vec<u8>)] = &[
            ("XFS", head_with(0, b"XFSB")),
            ("LUKS", head_with(0, b"LUKS\xba\xbe")),
            ("GPT", head_with(512, b"EFI PART")),
            ("an MBR partition table", head_with(510, b"\x55\xaa")),
            (
                "a tarball the host handed over by mistake",
                head_with(257, b"ustar"),
            ),
            ("a volume from a future ply", head_with(0, b"PLYVOL2")),
            // A VM killed part-way through mke2fs: the superblock is not
            // there yet, but something has been written.
            ("a half-written ext4", head_with(0, b"\x01\x02\x03\x04")),
            // btrfs, shaped the way mkfs.btrfs really shapes it. The
            // fixture this replaced was `\xeb\x63\x90` at offset 0 — an x86
            // boot-sector jump, which btrfs does not write — so the test
            // passed while the rule it was named for did not hold. See
            // `a_real_btrfs_volume_is_refused_even_though_its_first_4_kib_are_zero`.
            ("btrfs", btrfs_head()),
            // ZFS's L0 vdev label: 8 KiB blank pad, 8 KiB boot header, then
            // the nvlist. Same shape, a quarter of the distance out.
            ("ZFS", head_with(0x4000, b"\x00\x00\x00\x01")),
        ];
        for (what, head) in cases {
            assert_eq!(
                classify(head),
                Disk::Unrecognised,
                "{what} must be refused, not formatted"
            );
        }
    }

    #[test]
    fn a_real_btrfs_volume_is_refused_even_though_its_first_4_kib_are_zero() {
        // THE test this file was missing, and the reason `HEAD_BYTES` is
        // 68 KiB. `mkfs.btrfs` zeroes the leading 64 KiB, so a btrfs volume
        // is byte-identical to a fresh sparse file for as far as a 4 KiB
        // window can see — and `Blank` is the one disposition that runs
        // `mke2fs`. The previous version of this test asserted the same
        // conclusion over a fixture that was not btrfs at all, which is why
        // it passed while a real btrfs volume would have been destroyed.
        let head = btrfs_head();
        assert!(
            head[..4096].iter().all(|b| *b == 0),
            "a btrfs head really is all zeros where a 4 KiB check would look"
        );
        assert_eq!(
            &head[BTRFS_MAGIC_AT..BTRFS_MAGIC_AT + 8],
            b"_BHRfS_M",
            "and its magic really is at 0x10040"
        );
        // The conclusion first, so a narrowed window fails HERE and says
        // what it costs, rather than tripping the arithmetic check below.
        assert_ne!(
            classify(&head),
            Disk::Blank,
            "a btrfs filesystem must never be mistaken for a disk to format"
        );
        assert_eq!(classify(&head), Disk::Unrecognised);
        // ...and the reason it is not `Blank` is the width of the window,
        // asserted at COMPILE time: a `HEAD_BYTES` that no longer reaches
        // 0x10048 must not build, never mind fail a test run, because the
        // failure it causes in production is `mke2fs` over a btrfs volume.
        const { assert!(HEAD_BYTES >= BTRFS_MAGIC_AT + 8) };
        // The same shape one layer down: ZFS's first non-zero bytes are its
        // L0 nvlist at 0x4000, also past a 4 KiB window.
        let mut zfs = vec![0u8; HEAD_BYTES];
        zfs[0x4000] = 0x01;
        assert_ne!(classify(&zfs), Disk::Blank);
    }

    #[test]
    fn the_blank_rule_reads_a_fixed_window_not_however_much_it_was_handed() {
        // `Blank` means "the first HEAD_BYTES are zero", not "everything you
        // gave me is zero": a caller that read more must not be able to make
        // a fresh volume look written-on, and a caller that read exactly the
        // window is the normal case.
        assert_eq!(classify(&vec![0u8; HEAD_BYTES]), Disk::Blank);
        let mut longer = vec![0u8; HEAD_BYTES * 2];
        assert_eq!(classify(&longer), Disk::Blank, "a longer all-zero read");
        longer[HEAD_BYTES] = 1;
        assert_eq!(
            classify(&longer),
            Disk::Blank,
            "a byte past the window is outside the rule, not a silent third answer"
        );
    }

    #[test]
    fn a_short_head_is_refused_because_the_reader_promised_never_to_produce_one() {
        // `disk_head` returns HEAD_BYTES bytes or nothing, so this is
        // unreachable — and it is the branch that used to mean "format it",
        // so it is worth an assertion that it now means the opposite.
        assert_eq!(classify(&[]), Disk::Unrecognised);
        assert_eq!(classify(&[0u8; 16]), Disk::Unrecognised);
        assert_eq!(classify(&vec![0u8; HEAD_BYTES - 1]), Disk::Unrecognised);
    }

    #[test]
    fn a_refusal_names_the_device_and_says_what_to_do_about_it() {
        // A guest that refuses to boot and does not say how to unwedge it is
        // a support ticket; there is no e2fsck in this initramfs and no shell
        // to run one from, so the only remedy is on the host.
        let foreign = refusal("/data", "/dev/vdc", Disk::Foreign).expect("a refusal");
        assert!(foreign.contains("/dev/vdc") && foreign.contains("/data"));
        assert!(
            foreign.contains("image layer") || foreign.contains("spec disk"),
            "the foreign case has a better diagnosis than 'unknown bytes': {foreign}"
        );
        let unknown = refusal("/data", "/dev/vdc", Disk::Unrecognised).expect("a refusal");
        assert!(unknown.contains("/dev/vdc") && unknown.contains("/data"));
        assert!(
            unknown.contains("remove"),
            "say the remedy, do not leave it to be guessed: {unknown}"
        );
        // The two dispositions that are not refusals must not produce one.
        assert_eq!(refusal("/data", "/dev/vdc", Disk::Blank), None);
        assert_eq!(refusal("/data", "/dev/vdc", Disk::Ext4), None);
    }

    #[test]
    fn a_volume_that_will_not_mount_names_the_remedy_too() {
        // The specific way this happens: a VM killed part-way through
        // mke2fs leaves a superblock behind, so the next boot classifies the
        // disk as ext4, skips formatting, and cannot mount it either — and
        // the instance can never boot again until somebody deletes the file.
        let err = "mount(/dev/vdc, /data, ext4): EINVAL";
        let msg = unmountable("/data", "/dev/vdc", err, false);
        assert!(msg.contains("/dev/vdc") && msg.contains("/data"));
        assert!(msg.contains("EINVAL"), "keep the underlying error: {msg}");
        assert!(msg.contains("remove"), "say the remedy: {msg}");
        // A volume formatted on THIS boot that will not mount is a different
        // bug, and telling that user to delete the file sends them after the
        // wrong thing — there is nothing on it to lose and nothing to fix by
        // making another one.
        let just_made = unmountable("/data", "/dev/vdc", err, true);
        assert!(just_made.contains("EINVAL"));
        assert!(
            !just_made.contains("remove"),
            "do not send a user deleting files over a kernel/feature mismatch: {just_made}"
        );
    }

    #[test]
    fn the_format_arguments_pin_the_block_size_and_refuse_quota() {
        // Both of these are the whole reason this constant exists rather
        // than a bare `-t ext4`; a future edit that drops either one is a
        // volume with 1 KiB blocks for life, or a volume that cannot mount.
        let args = MKE2FS_ARGS.join(" ");
        assert!(args.contains("-b 4096"), "1 KiB blocks are forever: {args}");
        assert!(args.contains("^quota"), "CONFIG_QUOTA=n: {args}");
    }

    fn head_with(at: usize, bytes: &[u8]) -> Vec<u8> {
        let mut head = vec![0u8; HEAD_BYTES];
        head[at..at + bytes.len()].copy_from_slice(bytes);
        head
    }
}

#[cfg(target_os = "linux")]
pub use linux::{chown_at, disk_head, format, grow, mount_at};

/// The syscall half. Linux-only by nature — `mount(2)`'s signature and flags
/// are not portable — while everything above it is pure and compiles (and is
/// tested) everywhere, which is what keeps Linux CI covering the decisions.
#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;

    /// The real first `HEAD_BYTES` bytes of `dev`, or `None`.
    ///
    /// Named for the constant and not for a number: it was `first_4k` while
    /// `HEAD_BYTES` was 4096, and a name that has to be re-checked against a
    /// constant every time it changes is a name that will eventually lie.
    ///
    /// `None` covers both "no such device" and "a short read", and the
    /// caller must treat both as "do not touch this disk". The exactness is
    /// the contract: `classify` answers `Unrecognised` for a head shorter
    /// than `HEAD_BYTES`, so a partial read cannot be mistaken for a blank
    /// disk — but only because this function never returns one. The
    /// invariant is pinned by `a_short_device_reads_as_nothing_not_as_a_head`
    /// in `main.rs`, over a real 100-byte file.
    pub fn disk_head(dev: &str) -> Option<Vec<u8>> {
        crate::read_exact_head(dev, super::HEAD_BYTES)
    }

    /// `mke2fs` this device into an ext4 volume. Runs exactly once per
    /// volume, ever — on the boot that finds `Disk::Blank`, which is the one
    /// and only disposition `classify` lets through to here.
    pub fn format(dev: &str) -> Result<(), String> {
        let mut args: Vec<&str> = vec!["mke2fs"];
        args.extend_from_slice(super::MKE2FS_ARGS);
        args.push(dev);
        match crate::run("/sbin/mke2fs", &args) {
            0 => Ok(()),
            127 => Err(format!(
                "/sbin/mke2fs is not executable in this initramfs — the kernel keg was packed without e2fsprogs, so {dev} cannot be formatted"
            )),
            code => Err(format!("/sbin/mke2fs exited {code} for {dev}")),
        }
    }

    /// Grow the filesystem to the device's current size. A manifest may raise
    /// a volume's size between runs and the host simply extends the backing
    /// file; without this the extra bytes are invisible to the app forever.
    ///
    /// Called with no size argument on purpose: `resize2fs <dev>` means "as
    /// large as the device is", so the guest never has to be told a number
    /// and can never be told a wrong one.
    ///
    /// Best-effort by design: `resize2fs` refuses a filesystem that wants
    /// `e2fsck` first, and a volume that mounts at its old size is far better
    /// than a VM that will not boot.
    pub fn grow(dev: &str) -> Result<(), String> {
        match crate::run("/sbin/resize2fs", &["resize2fs", dev]) {
            0 => Ok(()),
            code => Err(format!("/sbin/resize2fs exited {code} for {dev}")),
        }
    }

    /// Mount an ext4 volume at an absolute path that already exists.
    pub fn mount_at(dev: &str, path: &str) -> Result<(), String> {
        if crate::mount(dev, path, "ext4", 0, "") {
            Ok(())
        } else {
            Err(format!(
                "mount({dev}, {path}, ext4): {}",
                std::io::Error::last_os_error()
            ))
        }
    }

    /// Give the volume's root directory to the app's user.
    ///
    /// The mount point ONLY, deliberately — not a recursive walk. The Linux
    /// backend chowns exactly one directory (`ns/container.rs`'s
    /// `volume_targets` loop) and everything below it is created by the app
    /// as itself, so a walk would change nothing while costing a full tree
    /// traversal of a possibly enormous database volume on every single
    /// boot. A freshly formatted ext4 holds only `/` and `lost+found`, so
    /// the first boot's one `chown` is the whole job.
    pub fn chown_at(path: &str, uid: u32, gid: u32) -> Result<(), String> {
        let c = CString::new(path).map_err(|_| format!("path has a NUL: {path}"))?;
        // SAFETY: `c` is a valid NUL-terminated C string for the call.
        if unsafe { libc::chown(c.as_ptr(), uid, gid) } == 0 {
            Ok(())
        } else {
            Err(format!(
                "chown({path}, {uid}:{gid}): {}",
                std::io::Error::last_os_error()
            ))
        }
    }
}
