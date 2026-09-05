#!/bin/sh
# Boot the microVM kernel that was just built and check that a real userspace
# can actually run on it.
#
#     scripts/microvm-smoke.sh <Image> <mke2fs> <resize2fs> <workdir>
#
# scripts/build-microvm-kernel.sh calls this after `make Image`. It is a
# separate file so it can be re-run alone against an Image without a 5-minute
# kernel build in front of it.
#
# WHY IT EXISTS, in one paragraph, because the temptation to delete it will be
# real: the build script asserts ~50 `CONFIG_X=y` lines, and an assertion loop
# can only ever check what somebody already thought of. Two Critical defects
# went past a 39-assertion check -- a kernel with no futex/epoll/shmget, so no
# PostgreSQL, redis, node or Go binary would run, and an overlayfs that
# returned -EIO for `mkdir` -- and both produced a kernel that booted and
# passed every assertion. `=y` proves presence, not usability. One boot that
# performs the operations a guest performs catches the class; the next
# unforeseen option is caught by the same run, with no edit.
#
# Needs: qemu-system-aarch64 (apt-get install -y qemu-system-arm) and
#        mksquashfs (apt-get install -y squashfs-tools).
# Missing qemu is a LOUD SKIP, not a failure: this must not stop someone
# building a kernel on a machine that cannot run one.
set -eu

img="${1:?usage: microvm-smoke.sh <Image> <mke2fs> <resize2fs> <workdir>}"
mke2fs_bin="${2:?}"
resize2fs_bin="${3:?}"
work="${4:?}"

here=$(cd "$(dirname "$0")" && pwd)
TIMEOUT="${SMOKE_TIMEOUT:-300}"

warn_skip() {
    echo
    echo "!!! =====================================================================" >&2
    echo "!!! BOOT SMOKE TEST SKIPPED: $*" >&2
    echo "!!! The kernel was NOT booted. The config assertion loop proves that" >&2
    echo "!!! options are SET, not that a userspace can RUN: a kernel that passed" >&2
    echo "!!! 39 of those assertions has already shipped unable to run PostgreSQL," >&2
    echo "!!! redis, node or any Go binary, and with an overlayfs that returned" >&2
    echo "!!! -EIO from mkdir. Install the tools and re-run:" >&2
    echo "!!!     sudo apt-get install -y qemu-system-arm squashfs-tools" >&2
    echo "!!!     $0 '$img' '$mke2fs_bin' '$resize2fs_bin' '$work'" >&2
    echo "!!! =====================================================================" >&2
    echo
}

command -v qemu-system-aarch64 >/dev/null 2>&1 || {
    warn_skip "qemu-system-aarch64 is not installed"
    exit 0
}
command -v mksquashfs >/dev/null 2>&1 || {
    warn_skip "mksquashfs is not installed (the overlay/squashfs checks are the
!!! ones that catch the -EIO defect, so this skip matters)"
    exit 0
}
command -v gcc >/dev/null 2>&1 || { echo "error: no gcc for the smoke init" >&2; exit 1; }

rm -rf "$work"
mkdir -p "$work"

# --- the guest program ----------------------------------------------------
# Static, host gcc, no Rust: it must build and run before ply-guest-init
# exists and must never share code with it, or a bug in the guest init would
# hide behind the same bug in its own test.
gcc -static -O2 -Wall -Wextra -o "$work/smoke-init" "$here/microvm-smoke-init.c"

# --- the lower layer: one zstd squashfs, shaped like an image layer -------
lay="$work/layer"
mkdir -p "$lay/optest" "$lay/renameme"
printf 'hello from the layer\n' > "$lay/hello"
printf 'keep\n'                 > "$lay/optest/keep"
printf 'inside renameme\n'      > "$lay/renameme/f"
printf 'x\n'                    > "$lay/xattrfile"
# user.* on the SOURCE file, so mksquashfs bakes it into the layer: the guest
# reading it back is the only proof CONFIG_SQUASHFS_XATTR is on.
python3 -c 'import os,sys; os.setxattr(sys.argv[1], b"user.plysmoke", b"ok")' \
    "$lay/xattrfile" 2>/dev/null || echo "note: could not set a user xattr on $lay/xattrfile (the squashfs_xattr check will fail)" >&2
# zstd is what `ply build` writes; -comp zstd also proves SQUASHFS_ZSTD.
mksquashfs "$lay" "$work/layer.sqfs" -comp zstd -noappend -no-progress -quiet 2>/dev/null \
    || mksquashfs "$lay" "$work/layer.sqfs" -comp zstd -noappend -no-progress

# --- a blank disk for the ext4 volume check -------------------------------
: > "$work/vol.img"
dd if=/dev/zero of="$work/vol.img" bs=1M count=64 status=none

# --- the smoke initramfs: the same packer the real build uses -------------
python3 "$here/mkinitramfs.py" "$work/smoke-init" "$work/smoke.cpio" \
    --extra "mke2fs=$mke2fs_bin" --extra "resize2fs=$resize2fs_bin" >/dev/null
python3 "$here/mkinitramfs.py" --verify "$work/smoke.cpio" \
    --require sbin/mke2fs --require sbin/resize2fs >/dev/null

# --- boot it --------------------------------------------------------------
# -M virt + virtio-*-device (not -pci): this kernel has no PCI at all, which
# is also how the HVF VMM in Task 8 wires its devices.
# Two virtconsole ports on purpose: hvc0 is the app's stdout, and the second
# is there to show what the control port looks like from inside the guest.
echo "smoke: booting $(basename "$img") under qemu (tcg, up to ${TIMEOUT}s)..."
set +e
timeout "$TIMEOUT" qemu-system-aarch64 \
    -M virt -cpu cortex-a57 -accel tcg -smp 2 -m 512 -nographic -no-reboot \
    -kernel "$img" \
    -initrd "$work/smoke.cpio" \
    -append "console=ttyAMA0 panic=-1 loglevel=4" \
    -drive "file=$work/layer.sqfs,if=none,format=raw,readonly=on,id=layer" \
    -device virtio-blk-device,drive=layer \
    -drive "file=$work/vol.img,if=none,format=raw,id=vol" \
    -device virtio-blk-device,drive=vol \
    -device virtio-rng-device \
    -chardev file,id=hvc0chr,path="$work/hvc0.out" \
    -chardev null,id=hvc1chr \
    -device virtio-serial-device,max_ports=8 \
    -device virtconsole,chardev=hvc0chr \
    -device virtconsole,chardev=hvc1chr \
    < /dev/null > "$work/console.log" 2>&1
qrc=$?
set -e

echo "--- guest console ---------------------------------------------------"
tr -d '\r' < "$work/console.log" 2>/dev/null | grep -a -E \
    '^(SMOKE-|PASS |FAIL |WARN |INFO |Kernel panic|BUG:|Unable to handle)' || {
    echo "(no smoke output at all -- full console log follows)"
    cat "$work/console.log"
}
echo "---------------------------------------------------------------------"

fails=$(grep -a -c '^FAIL ' "$work/console.log" || true)
[ -n "$fails" ] || fails=0
warns=$(grep -a -c '^WARN ' "$work/console.log" || true)
[ -n "$warns" ] || warns=0

if ! grep -aq '^SMOKE-DONE' "$work/console.log"; then
    echo "error: the guest never reached SMOKE-DONE (qemu exit $qrc)." >&2
    echo "       That is a panic, a hang, or an init that died before it could" >&2
    echo "       say why. Full console log: $work/console.log" >&2
    tail -40 "$work/console.log" >&2
    exit 1
fi

if [ "$fails" -gt 0 ]; then
    echo "error: $fails smoke check(s) FAILED on the kernel that was just built." >&2
    grep -a '^FAIL ' "$work/console.log" >&2
    echo "       Full console log: $work/console.log" >&2
    exit 1
fi

echo "smoke:     all checks passed ($warns warning(s)); log $work/console.log"
