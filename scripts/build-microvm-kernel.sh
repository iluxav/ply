#!/bin/sh
# Build the ply microVM kernel and its initramfs.
#
# Runs on any aarch64 Linux. On a Mac, inside Lima:
#     limactl start default
#     lima bash -lc 'OUT=$HOME/microvm-build bash /Users/iluxav/Documents/ply/scripts/build-microvm-kernel.sh'
#
# Keep OUT on VM-LOCAL DISK, not on the 9p/virtiofs mount of the repo: a
# kernel build writes ~1 GB of small files and the shared mount makes it
# several times slower.
#
# Produces, in $OUT:
#   microvm-kernel-<kver>-linux-arm64.img   raw arm64 Image, ~5 MiB
#   initramfs.cpio                          guest init + e2fsprogs + dev nodes,
#                                           ~2.4 MiB of it stripped e2fsprogs
#   keg/boot/{microvm-kernel.img,initramfs.cpio}
#                                           the same two files, staged
#   keg/microvm-kernel-<kver>-linux-arm64.img
#                                           the ply/microvm-kernel KEG -- a ply
#                                           image, and the thing `ply push`
#                                           takes. Same basename as the raw
#                                           Image above, different directory:
#                                           the one in keg/ is squashfs, the
#                                           one beside it is a bare kernel.
#
# The kernel is part of the RUNTIME, not of any app: the ply binary pins one
# version in runtime/vm/kernel.rs and no lockfile ever mentions it.
#
# What this script does, in order, and why the order matters:
#   1. unit-test the initramfs packer            (a binary format, by hand)
#   2. build the guest init and static e2fsprogs
#   3. pack the initramfs, then READ IT BACK and assert its contents
#   4. fetch + SHA-256-VERIFY the kernel source, extract it atomically
#   5. build the Image reproducibly
#   6. assert every required option in the BUILT .config
#   7. BOOT the Image under qemu and check a real userspace works
#   8. only then publish $OUT/microvm-kernel-*.img
#   9. assemble the keg -- a ply image carrying those two files -- and read
#      its payload back OUT of the finished image before naming `ply push`
# Nothing is published until every check has passed, because a plausible-
# looking Image that a developer's PLY_MICROVM_KERNEL already points at is
# worse than no Image at all.
#
# Env knobs: KVER, E2FSVER, OUT, JOBS, CARGO_TARGET_DIR (honoured, and the
# Lima gate sets it), SKIP_GUEST_INIT (a stub init, for proving the kernel
# half alone -- it also skips the keg, which must never be built around a
# do-nothing init), SKIP_SMOKE (do not; see scripts/microvm-smoke.sh for what
# it costs you), SKIP_KEG, PLY_BIN (an existing ply binary instead of
# `cargo run -p ply-cli`).
set -eu

KVER="${KVER:-6.12.0}"
E2FSVER="${E2FSVER:-1.47.4}"
OUT="${OUT:-out}"
JOBS="${JOBS:-$(nproc 2>/dev/null || echo 4)}"

# A pinned version without a pinned digest is half a pin: this Image is the
# trust root of every guest on every Mac, and `curl | tar` would run whatever
# the CDN, a proxy or a DNS answer happened to hand back. From
# https://cdn.kernel.org/pub/linux/kernel/v6.x/sha256sums.asc and
# https://mirrors.edge.kernel.org/pub/linux/kernel/people/tytso/e2fsprogs/v<v>/sha256sums.asc
# When bumping KVER or E2FSVER, the digest must be bumped with it in the same
# edit -- the build fails loudly rather than silently using a new tarball.
KSHA256="${KSHA256:-b1a2562be56e42afb3f8489d4c2a7ac472ac23098f1ef1c1e40da601f54625eb}"
E2FSSHA256="${E2FSSHA256:-fd5bf388cbdbe006a3d3b318d983b2948382440acc85a87f1e7d108653e8db0b}"

die() { echo "error: $*" >&2; exit 1; }

[ "$(uname -s)" = Linux ] || die "run this on Linux (on a Mac: limactl start default && lima bash $0)"
[ "$(uname -m)" = aarch64 ] || die "arm64 only (the guest is linux-arm64), this host is $(uname -m)"

# Fail here with a name rather than halfway through a kernel build with a
# compiler error nobody reads.
# unsquashfs is in this list, not behind an `if`, on purpose: it is the ONLY
# check that reads the keg's payload back out of the finished image, and
# `ply build` writes squashfs with a pure-Rust writer, so nothing pulls
# squashfs-tools in transitively and a stock Lima/Ubuntu image has not got it.
# Skipping the check when the tool is absent made the unverified path the
# LIKELY one. `apt install squashfs-tools` is a one-line answer at minute
# zero; an unverified keg is a Mac that will not boot, found three tasks later.
for tool in curl tar make gcc ld flex bison bc python3 sed awk file sha256sum unsquashfs; do
    command -v "$tool" >/dev/null 2>&1 || die "missing build tool: $tool"
done
[ -e /usr/include/openssl/opensslv.h ] || die "missing openssl headers (apt install libssl-dev)"
[ -e /usr/include/libelf.h ] || die "missing libelf headers (apt install libelf-dev)"

mkdir -p "$OUT"
here=$(cd "$(dirname "$0")/.." && pwd)

# --- reproducibility ------------------------------------------------------
# Without these the Image embeds the builder's username, hostname, wall-clock
# time and an incrementing build counter, so two builds of the same source at
# the same version are different bytes -- which makes the keg's digest
# meaningless as an identity, leaks `whoami@their-laptop` into every guest's
# `uname -a`, and turns "is this the kernel I think it is?" into a question
# nobody can answer. SOURCE_DATE_EPOCH is 2026-01-01T00:00:00Z: a constant,
# not "now", because "now" is the whole problem.
export KBUILD_BUILD_USER="${KBUILD_BUILD_USER:-ply}"
export KBUILD_BUILD_HOST="${KBUILD_BUILD_HOST:-microvm}"
export KBUILD_BUILD_VERSION="${KBUILD_BUILD_VERSION:-1}"
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-1767225600}"
export KBUILD_BUILD_TIMESTAMP="${KBUILD_BUILD_TIMESTAMP:-$(date -u -d "@$SOURCE_DATE_EPOCH" 2>/dev/null || echo "Thu Jan  1 00:00:00 UTC 2026")}"

# --- the initramfs packer's own tests -------------------------------------
# It writes a binary format by hand and the kernel is the only other reader,
# so a mistake there is a guest that does not boot and cannot say so. Cheap,
# and it runs before anything expensive.
python3 "$here/scripts/test_mkinitramfs.py" 2>&1 | tail -3

# --- helper: fetch a tarball and prove it is the one we pinned ------------
fetch_verify() {
    url="$1"; want="$2"; dest="$3"
    if [ -f "$dest" ] && [ "$(sha256sum < "$dest" | cut -d' ' -f1)" = "$want" ]; then
        echo "cached:    $(basename "$dest") (sha256 ok)"
        return 0
    fi
    echo "fetching:  $url"
    # .part, so an interrupted download is never mistaken for a complete one.
    curl -fsSL "$url" -o "$dest.part" || die "download failed: $url"
    got=$(sha256sum < "$dest.part" | cut -d' ' -f1)
    if [ "$got" != "$want" ]; then
        rm -f "$dest.part"
        die "sha256 mismatch for $url
       want $want
       got  $got
       Either the pin is stale (bump the constant in this script IN THE SAME
       EDIT as the version) or something served you a different tarball."
    fi
    mv "$dest.part" "$dest"
    echo "verified:  $(basename "$dest") sha256 $got"
}

# --- helper: extract so that a half-extraction can never be reused --------
# `if [ ! -d "$src" ]` treats EXISTENCE as COMPLETENESS. Interrupt a
# `curl | tar` and the next run finds a truncated tree, skips the download,
# and silently builds from whatever made it to disk. Extract to a scratch
# path and rename the finished tree into place, then stamp it.
extract_once() {
    tarball="$1"; final="$2"; topdir="$3"
    [ -f "$final/.ply-extracted" ] && return 0
    rm -rf "$final" "$final.extracting"
    mkdir -p "$final.extracting"
    tar xJf "$tarball" -C "$final.extracting" || die "extract failed: $tarball"
    [ -d "$final.extracting/$topdir" ] || die "$tarball did not contain $topdir/"
    mv "$final.extracting/$topdir" "$final"
    rmdir "$final.extracting" 2>/dev/null || rm -rf "$final.extracting"
    : > "$final/.ply-extracted"
}

# --- the guest init: one static arm64 binary, no runtime deps -------------
# rustc's own bundled lld links musl targets, so this needs no cross
# toolchain and no Homebrew package. Set the linker by ENV, never in a
# .cargo/config.toml: a config file would apply to the whole workspace and
# would break the release lane's `cross build` for the same target.
if [ -n "${SKIP_GUEST_INIT:-}" ]; then
    # Kernel-half-only run: a do-nothing init, so the initramfs and the
    # kernel recipe can be proven before ply-guest-init exists (Task 3).
    # pause(), not `for(;;);`: a spin loop pins a vCPU at 100% for as long as
    # the VM lives, which on a laptop is a fan at full speed and a battery
    # draining to prove nothing. pause() blocks in the kernel forever, which
    # is the same "do nothing" at zero cost.
    init="$OUT/stub-init"
    printf '#include <unistd.h>\nint main(void){for(;;)pause();}\n' > "$OUT/stub-init.c"
    gcc -static -Os -o "$init" "$OUT/stub-init.c"
    echo "guest init: STUB (SKIP_GUEST_INIT set) -> $init"
else
    rustup target add aarch64-unknown-linux-musl >/dev/null
    # opt-level=z is worth 64 KB on this init -- ~534,000 -> 468,552 bytes,
    # measured 2026-09-03 against the finished boot sequence (Task 6) -- and
    # every byte is unpacked into guest RAM on every single VM start. That
    # first figure is the one rounded number here; it was recorded that way.
    # By ENVIRONMENT, never a [profile.release.package] section: the
    # per-package override measured a THIRD LARGER than plain release
    # (464,136 vs 349,720 bytes). DO NOT COMPARE ACROSS THE TWO PAIRS: that
    # second pair is Task 3's earlier, much smaller init, so 464,136 next to
    # 468,552 makes the override look like a win when it is 33% worse on the
    # same code. A config file would also apply to the whole workspace and to
    # the release lane's cross build.
    CARGO_PROFILE_RELEASE_OPT_LEVEL=z \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=+crt-static" \
      cargo build --release --target aarch64-unknown-linux-musl -p ply-guest-init
    # cargo writes where CARGO_TARGET_DIR says, and the Lima gate sets it to
    # a VM-local path precisely so the Linux and macOS builds stop forcing
    # full rebuilds of each other. Hardcoding $here/target made this script
    # do a second, duplicate build into the repo's own tree.
    init="${CARGO_TARGET_DIR:-$here/target}/aarch64-unknown-linux-musl/release/ply-guest-init"
fi
[ -x "$init" ] || die "no guest init at $init"

# --- static e2fsprogs: the guest formats and grows its own volumes --------
# Built from source using e2fsprogs' OWN `mke2fs.static` / `resize2fs.static`
# targets, because there is no prebuilt static aarch64 mke2fs to fetch:
# Alpine's `e2fsprogs-static` package holds static *libraries* (libext2fs.a
# and friends) and no executable at all, and its plain `e2fsprogs` package
# ships a musl-DYNAMIC sbin/mke2fs with no resize2fs (that is in
# `e2fsprogs-extra`). Neither survives an initramfs that has no libc.
# ~1 min to build, and an exact pinned version instead of a mirror index
# that changes under us.
#
# mke2fs needs no /etc/mke2fs.conf here: when the file is absent it falls
# back to `mke2fs_default_profile`, the same table compiled in from
# mke2fs.conf.in at build time.
e2fs_src="$OUT/e2fsprogs-$E2FSVER"
mke2fs_bin="$OUT/e2fsprogs-bin/mke2fs"
resize2fs_bin="$OUT/e2fsprogs-bin/resize2fs"
if [ ! -x "$mke2fs_bin" ] || [ ! -x "$resize2fs_bin" ]; then
    fetch_verify \
        "https://mirrors.edge.kernel.org/pub/linux/kernel/people/tytso/e2fsprogs/v$E2FSVER/e2fsprogs-$E2FSVER.tar.xz" \
        "$E2FSSHA256" "$OUT/e2fsprogs-$E2FSVER.tar.xz"
    extract_once "$OUT/e2fsprogs-$E2FSVER.tar.xz" "$e2fs_src" "e2fsprogs-$E2FSVER"
    mkdir -p "$e2fs_src/build" "$OUT/e2fsprogs-bin"
    # --disable-* trims everything the guest never calls; the static link is
    # what configure probes for and enables in LDFLAGS_STATIC.
    (cd "$e2fs_src/build" && ../configure --disable-nls --disable-uuidd \
        --disable-fuse2fs --disable-e2initrd-helper --disable-testio-debug \
        --disable-debugfs --disable-imager >/dev/null)
    make -C "$e2fs_src/build" -j"$JOBS" libs >/dev/null
    make -C "$e2fs_src/build/misc" -j"$JOBS" mke2fs.static >/dev/null
    make -C "$e2fs_src/build/resize" -j"$JOBS" resize2fs.static >/dev/null
    cp "$e2fs_src/build/misc/mke2fs.static" "$mke2fs_bin"
    cp "$e2fs_src/build/resize/resize2fs.static" "$resize2fs_bin"
    # Debug info is 60% of these binaries (5.9 MiB -> 2.4 MiB) and the whole
    # initramfs is unpacked into guest RAM on every single VM start.
    strip "$mke2fs_bin" "$resize2fs_bin"
fi
for bin in "$mke2fs_bin" "$resize2fs_bin"; do
    [ -x "$bin" ] || die "e2fsprogs did not produce $bin"
    # A dynamic binary here would abort inside the guest with a missing
    # interpreter and no way to say so.
    file "$bin" 2>/dev/null | grep -q "statically linked" \
        || die "$bin is not statically linked"
done
echo "e2fsprogs: $(stat -c%s "$mke2fs_bin") + $(stat -c%s "$resize2fs_bin") bytes (static)"

python3 "$here/scripts/mkinitramfs.py" "$init" "$OUT/initramfs.cpio" \
    --extra "mke2fs=$mke2fs_bin" \
    --extra "resize2fs=$resize2fs_bin"

# Read the archive back and assert what the guest cannot boot without. The
# packer is the only writer of this format and the kernel is the only other
# reader, so "it wrote a file" is not evidence. /dev/null is here because it
# WAS missing once (ruling R0-4): Rust's std opens it for the standard
# descriptors and aborts before main() if it cannot, and the whole visible
# symptom was "Attempted to kill init!". That cost an hour.
python3 "$here/scripts/mkinitramfs.py" --verify "$OUT/initramfs.cpio" \
    --require sbin/mke2fs --require sbin/resize2fs

# --- the kernel -----------------------------------------------------------
# kernel.org names the .0 release `linux-6.12.tar.xz`, not `linux-6.12.0`,
# but KVER must stay three-component semver: it is the keg version the ply
# binary pins (`ply/microvm-kernel@6.12.0`). Derive the upstream name.
ktar=$(echo "$KVER" | sed 's/\.0$//')
kmajor=$(echo "$KVER" | cut -d. -f1)
src="$OUT/linux-$ktar"
fetch_verify "https://cdn.kernel.org/pub/linux/kernel/v$kmajor.x/linux-$ktar.tar.xz" \
    "$KSHA256" "$OUT/linux-$ktar.tar.xz"
extract_once "$OUT/linux-$ktar.tar.xz" "$src" "linux-$ktar"

make -C "$src" O=build ARCH=arm64 tinyconfig
cat "$here/kernel/microvm.config" >> "$src/build/.config"
make -C "$src" O=build ARCH=arm64 olddefconfig
make -C "$src" O=build ARCH=arm64 -j"$JOBS" Image

built="$src/build/arch/arm64/boot/Image"
[ -f "$built" ] || die "make Image produced no $built"

# Every option the guest init and the VMM depend on, asserted against the
# BUILT config rather than the fragment we appended. `olddefconfig` silently
# DROPS any option whose dependencies are unmet — no warning, the line is
# just absent from the result — so kernel/microvm.config is a REQUEST, not
# evidence. Measured on 6.12: VIRTIO_MMIO, SQUASHFS, SQUASHFS_XZ,
# SQUASHFS_ZSTD and TMPFS were all lost this way before their `menuconfig`
# parents were added to the fragment.
#
# Two checks, because they catch different mistakes: the first catches
# kconfig dropping something the fragment asked for, the second catches
# somebody deleting a load-bearing line FROM the fragment.
#
# WHAT THIS LOOP CANNOT CATCH — read this before trusting a green run:
#   * `=y` proves PRESENCE, not USABILITY. A parent can be `=y` with the
#     child that makes it work turned off. CONFIG_OVERLAY_FS=y with
#     CONFIG_TMPFS_XATTR=n is a mounted overlay that returns -EIO from
#     `mkdir`; CONFIG_SQUASHFS=y with CONFIG_SQUASHFS_XATTR=n silently drops
#     file capabilities out of every image layer.
#   * It can only assert what somebody already thought to list. The whole
#     syscall surface -- futex, epoll, shmget, flock, timerfd, inotify --
#     was absent from a kernel that passed 39 of these assertions, because
#     nobody had thought to list options that are `default y` upstream.
#   * It says nothing about what happens at boot.
# That is what the boot smoke test below is for, and why it runs before the
# Image is published rather than after.
required="$(sed -n 's/^\(CONFIG_[A-Z0-9_]*\)=y$/\1/p' "$here/kernel/microvm.config")
CONFIG_VIRTIO_MMIO CONFIG_VIRTIO_BLK CONFIG_VIRTIO_NET CONFIG_VIRTIO_CONSOLE
CONFIG_HVC_DRIVER CONFIG_SQUASHFS CONFIG_SQUASHFS_ZSTD CONFIG_OVERLAY_FS
CONFIG_EXT4_FS CONFIG_SERIAL_AMBA_PL011_CONSOLE CONFIG_BLK_DEV_INITRD
CONFIG_DEVTMPFS CONFIG_DEVTMPFS_MOUNT CONFIG_TMPFS CONFIG_BINFMT_ELF
CONFIG_NET CONFIG_UNIX
CONFIG_FUTEX CONFIG_EPOLL CONFIG_EVENTFD CONFIG_SIGNALFD CONFIG_TIMERFD
CONFIG_POSIX_TIMERS CONFIG_FILE_LOCKING CONFIG_SYSVIPC CONFIG_INOTIFY_USER
CONFIG_ADVISE_SYSCALLS CONFIG_RSEQ CONFIG_AIO CONFIG_IO_URING
CONFIG_MEMBARRIER CONFIG_FHANDLE CONFIG_SECCOMP
CONFIG_TMPFS_XATTR CONFIG_SQUASHFS_XATTR CONFIG_OVERLAY_FS_REDIRECT_DIR
CONFIG_BUG CONFIG_KALLSYMS CONFIG_PRINTK_TIME"

missing=""
# Unquoted on purpose: kconfig symbol names cannot contain whitespace, so
# splitting the list into words is exactly the iteration wanted.
for opt in $required; do
    grep -q "^$opt=y" "$src/build/.config" || missing="$missing $opt"
done
if [ -n "$missing" ]; then
    # All of them at once: finding these one rebuild at a time is the
    # expensive way to learn what tinyconfig turned off.
    for opt in $missing; do echo "  not set in the built kernel: $opt" >&2; done
    die "the built kernel is missing required options (see above)"
fi
echo "config:    all required options present ($(echo $required | wc -w) checked)"

# The banner is the reproducibility check: no username, no hostname, no
# wall-clock time, no incrementing #N. If any of those reappear, two builds
# of ply/microvm-kernel@$KVER are different bytes and the version stops
# meaning anything.
banner=$(strings "$built" 2>/dev/null | grep -m1 '^Linux version ' || true)
echo "banner:    ${banner:-(not found -- CONFIG_KALLSYMS or strings?)}"
case "$banner" in
    *"$KBUILD_BUILD_USER@$KBUILD_BUILD_HOST"*) : ;;
    "") echo "warning: could not read the version banner out of the Image" >&2 ;;
    *) die "the Image embeds a builder identity: $banner
       KBUILD_BUILD_USER/HOST did not take effect, so this build is not reproducible." ;;
esac

# --- boot it before publishing it -----------------------------------------
if [ -n "${SKIP_SMOKE:-}" ]; then
    echo "warning: SKIP_SMOKE set -- the Image below has NOT been booted." >&2
else
    sh "$here/scripts/microvm-smoke.sh" "$built" "$mke2fs_bin" "$resize2fs_bin" \
        "$OUT/smoke"
fi

# --- publish, last ---------------------------------------------------------
# After the assertions and the boot, never before: a failed check that still
# leaves a plausible out/microvm-kernel-*.img is worse than no image, because
# a developer's PLY_MICROVM_KERNEL may already point at that path and will
# pick up the broken one without a word.
img="$OUT/microvm-kernel-$KVER-linux-arm64.img"
cp "$built" "$img.new" && mv "$img.new" "$img"
echo "kernel:    $(stat -c%s "$img") bytes -> $img"
echo "initramfs: $(stat -c%s "$OUT/initramfs.cpio") bytes -> $OUT/initramfs.cpio"

# --- the keg ---------------------------------------------------------------
# An ordinary ply image whose payload is the two files the VM backend needs.
# `runtime/vm/kernel.rs` fetches it with `catalog::fetch_keg_image` (a keg has
# no entrypoint, and the app path refuses kegs on purpose), extracts it into
# the store, and reads the two files from
#
#     /opt/microvm-kernel-<version>/boot/{microvm-kernel.img,initramfs.cpio}
#
# Both halves of that path are load-bearing:
#   * `/opt/<name>-<version>` is where `ply build` packs a KEG's directory
#     (an app gets `/opt/<name>`), decided by build.rs, not by us.
#   * `boot/` is not decoration. `ply build` never packs a TOP-LEVEL `*.img`
#     -- that is how it keeps a build's own output out of the next image --
#     so `microvm-kernel.img` staged at the keg root would be dropped
#     SILENTLY: a keg that builds, pushes and resolves with no kernel inside
#     it. One directory down it packs, and /boot is where a Linux system
#     keeps a kernel and an initramfs anyway.
# `kernel::keg_payload_dir` and its unit test carry the same layout. The two
# must move together.
if [ -n "${SKIP_GUEST_INIT:-}" ]; then
    echo
    echo "keg:       SKIPPED -- this build's init is the do-nothing stub, and a keg" >&2
    echo "           built around it would boot every microVM into nothing." >&2
elif [ -n "${SKIP_KEG:-}" ]; then
    echo
    echo "keg:       SKIPPED (SKIP_KEG set)"
else
    keg="$OUT/keg"
    rm -rf "$keg"
    mkdir -p "$keg/boot"
    cp "$img" "$keg/boot/microvm-kernel.img"
    cp "$OUT/initramfs.cpio" "$keg/boot/initramfs.cpio"
    cp "$here/kernel/microvm-kernel.toml" "$keg/ply.toml"

    # PLY_BIN is for a machine that already has ply; otherwise build the one
    # in this tree, so the keg is packed by the same code that will read it.
    if [ -n "${PLY_BIN:-}" ]; then
        "$PLY_BIN" build "$keg" --arch arm64
    else
        cargo run --release -p ply-cli -- build "$keg" --arch arm64
    fi

    kegimg="$keg/microvm-kernel-$KVER-linux-arm64.img"
    [ -f "$kegimg" ] || die "ply build produced no $kegimg
       (does kernel/microvm-kernel.toml still say version = \"$KVER\"?)"

    # Read the payload back OUT of the image. `ply build` reports what it
    # packed, but the failure this guards against -- a file silently filtered
    # out -- looks exactly like a successful build from the outside, and the
    # next thing to notice would be a Mac failing to boot a VM.
    listing=$(unsquashfs -l "$kegimg" 2>/dev/null) || die "cannot list $kegimg"
    for want in microvm-kernel.img initramfs.cpio; do
        echo "$listing" \
            | grep -qx "squashfs-root/opt/microvm-kernel-$KVER/boot/$want" \
            || die "the keg does not contain /opt/microvm-kernel-$KVER/boot/$want
       -- \`ply build\` filtered it out. Nothing on a Mac would report this."
    done
    echo "keg:       payload verified inside the image (unsquashfs -l)"

    echo "keg:       $(stat -c%s "$kegimg") bytes -> $kegimg"
    if [ -n "${SKIP_SMOKE:-}" ]; then
        echo "keg:       WARNING -- the kernel in this keg was NEVER BOOTED (SKIP_SMOKE)." >&2
        echo "           Do not push it; rebuild without SKIP_SMOKE first." >&2
    fi
    echo
    echo "Point this machine at the local kernel (no registry needed):"
    echo "    export PLY_MICROVM_KERNEL=$keg/boot"
    echo
    echo "Publishing is the owner's step -- the manifest already says owner = \"ply\":"
    echo "    ply push $kegimg"
fi
