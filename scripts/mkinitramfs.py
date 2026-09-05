#!/usr/bin/env python3
"""Build (and check) the microVM initramfs: a newc cpio holding the guest
init, the device nodes it needs, and static e2fsprogs for volume formatting.

Written by hand rather than with cpio(1) because the archive must contain
CHARACTER DEVICE NODES, which no unprivileged tar or cpio can create.

Usage:
    mkinitramfs.py <init-binary> <out.cpio> [--extra NAME=PATH ...]
    mkinitramfs.py --verify <cpio> [--require PATH ...]

`--verify` re-reads an archive this script wrote and asserts the entries the
guest cannot boot without. It lives here, next to the writer, because both
halves need the same knowledge of the newc layout; scripts/build-microvm-
kernel.sh calls it on every build. scripts/test_mkinitramfs.py tests both.
"""

import sys

USAGE = (
    "usage: mkinitramfs.py <init-binary> <out.cpio> [--extra NAME=PATH ...]\n"
    "       mkinitramfs.py --verify <cpio> [--require PATH ...]"
)

MAGIC = b"070701"
# 13 fields, 8 hex digits each, after the 6-byte magic.
HEADER_LEN = len(MAGIC) + 13 * 8

FIELD_NAMES = (
    "ino", "mode", "uid", "gid", "nlink", "mtime", "filesize",
    "devmajor", "devminor", "rdevmajor", "rdevminor", "namesize", "check",
)

S_IFMT = 0o170000
S_IFREG = 0o100000
S_IFDIR = 0o040000
S_IFCHR = 0o020000


class InitramfsError(Exception):
    """A problem with the arguments or the archive, reported as one line."""


def _hex8(field, value):
    """One 8-hex-digit newc field.

    The width is FIXED at 8, not a minimum: `f"{f:08X}"` happily emits NINE
    characters for any value >= 2**32, which does not fail, does not warn,
    and shifts every byte of every later record by one — an archive the
    kernel then rejects, or worse, misreads. `filesize` is the field that can
    realistically get there (a >= 4 GiB init), and the newc format simply
    cannot express it, so refuse instead of writing a corrupt archive.
    """
    if not isinstance(value, int) or not 0 <= value < 2**32:
        raise InitramfsError(
            f"newc field {field!r} = {value!r} does not fit in 8 hex digits "
            "(0 .. 4294967295); the newc format cannot represent it"
        )
    return f"{value:08X}".encode()


def _pad4(n):
    return b"\0" * ((4 - (n % 4)) % 4)


def entry(name, mode, filedata=b"", dev=(0, 0), ino=[100]):
    """One newc record. Fields, in the order the format fixes them:
    ino, mode, uid, gid, nlink, mtime, filesize, devmajor, devminor,
    rdevmajor, rdevminor, namesize, check. A device node carries its
    major/minor in *rdev*, which is why `dev` lands in slots 10 and 11.
    """
    ino[0] += 1
    fields = [ino[0], mode, 0, 0, 1, 0, len(filedata), 0, 0, dev[0], dev[1], len(name) + 1, 0]
    out = MAGIC + b"".join(_hex8(n, f) for n, f in zip(FIELD_NAMES, fields))
    out += name.encode() + b"\0"
    out += _pad4(len(out))
    out += filedata
    out += _pad4(len(filedata))
    return out


def read_file(path, what):
    try:
        with open(path, "rb") as f:
            return f.read()
    except OSError as e:
        raise InitramfsError(f"cannot read {what} {path!r}: {e.strerror}") from None


def build(init_path, extras):
    """Return the bytes of the archive. `extras` is an ordered NAME -> PATH map."""
    out = b""
    out += entry("dev", 0o040755)
    out += entry("dev/console", 0o020600, dev=(5, 1))
    # /dev/null is NOT optional. The kernel starts init with no open fds when
    # it could not open /dev/console, and Rust's std then opens /dev/null for
    # the missing standard descriptors and ABORTS if it cannot — the guest
    # dies before main() with a bare "Attempted to kill init!". Cost an hour
    # in the milestone 0 spike; see the spike result document.
    out += entry("dev/null", 0o020666, dev=(1, 3))
    out += entry("proc", 0o040755)
    out += entry("sys", 0o040755)
    out += entry("run", 0o040755)
    out += entry("sbin", 0o040755)
    for name, path in sorted(extras.items()):
        out += entry(f"sbin/{name}", 0o100755, read_file(path, f"--extra {name}"))
    out += entry("init", 0o100755, read_file(init_path, "init binary"))
    out += entry("TRAILER!!!", 0)
    return out


def parse(blob):
    """Parse a newc archive into [(name, header dict, data)], TRAILER excluded."""
    entries = []
    off = 0
    while off < len(blob):
        if blob[off:off + len(MAGIC)] != MAGIC:
            raise InitramfsError(f"not a newc record at offset {off}")
        vals = {}
        for i, key in enumerate(FIELD_NAMES):
            start = off + len(MAGIC) + i * 8
            raw = blob[start:start + 8]
            if len(raw) != 8:
                raise InitramfsError(f"truncated header at offset {off}")
            try:
                vals[key] = int(raw, 16)
            except ValueError:
                raise InitramfsError(
                    f"field {key!r} at offset {start} is not 8 hex digits: {raw!r} "
                    "(a field wider than 8 digits shifts every later record)"
                ) from None
        name_off = off + HEADER_LEN
        name = blob[name_off:name_off + vals["namesize"] - 1].decode("utf-8", "replace")
        data_off = name_off + vals["namesize"]
        data_off += (4 - (data_off % 4)) % 4
        data = blob[data_off:data_off + vals["filesize"]]
        if len(data) != vals["filesize"]:
            raise InitramfsError(f"truncated file data for {name!r}")
        off = data_off + vals["filesize"]
        off += (4 - (off % 4)) % 4
        if name == "TRAILER!!!":
            break
        entries.append((name, vals, data))
    return entries


# The entries the guest cannot boot without, as (path, S_IF*, rdev-or-None).
# dev/null is here for ruling R0-4: it went missing once and cost an hour.
REQUIRED = (
    ("dev", S_IFDIR, None),
    ("dev/console", S_IFCHR, (5, 1)),
    ("dev/null", S_IFCHR, (1, 3)),
    ("init", S_IFREG, None),
)


def verify(blob, require=()):
    """Return a list of human-readable problems; empty means the archive is good."""
    problems = []
    try:
        entries = parse(blob)
    except InitramfsError as e:
        return [str(e)]
    found = {name: vals for name, vals, _ in entries}
    for path, kind, rdev in list(REQUIRED) + [(p, S_IFREG, None) for p in require]:
        vals = found.get(path)
        if vals is None:
            problems.append(f"missing {path}")
            continue
        if vals["mode"] & S_IFMT != kind:
            problems.append(
                f"{path}: mode {vals['mode']:07o} is not a "
                f"{'directory' if kind == S_IFDIR else 'character device' if kind == S_IFCHR else 'regular file'}"
            )
        if rdev is not None and (vals["rdevmajor"], vals["rdevminor"]) != rdev:
            problems.append(
                f"{path}: device {vals['rdevmajor']}:{vals['rdevminor']}, "
                f"want {rdev[0]}:{rdev[1]}"
            )
        if kind == S_IFREG and not vals["mode"] & 0o111:
            problems.append(f"{path}: mode {vals['mode']:07o} is not executable")
        if kind == S_IFREG and vals["filesize"] == 0:
            problems.append(f"{path}: empty")
    return problems


def parse_args(argv):
    """Return ("build", init, out, extras) or ("verify", cpio, require)."""
    if argv[:1] == ["--verify"]:
        if len(argv) < 2:
            raise InitramfsError("--verify needs a cpio path")
        cpio, rest, require = argv[1], argv[2:], []
        i = 0
        while i < len(rest):
            if rest[i] == "--require":
                if i + 1 >= len(rest):
                    raise InitramfsError("--require needs a PATH")
                require.append(rest[i + 1])
                i += 2
            else:
                raise InitramfsError(f"unexpected argument {rest[i]!r}")
        return ("verify", cpio, require)

    if len(argv) < 2:
        raise InitramfsError(USAGE)
    init_path, out_path = argv[0], argv[1]
    for p in (init_path, out_path):
        if p.startswith("--"):
            raise InitramfsError(USAGE)
    extras = {}
    i = 2
    while i < len(argv):
        if argv[i] != "--extra":
            raise InitramfsError(f"unexpected argument {argv[i]!r}")
        if i + 1 >= len(argv):
            raise InitramfsError("--extra needs NAME=PATH")
        if "=" not in argv[i + 1]:
            raise InitramfsError(f"--extra wants NAME=PATH, got {argv[i + 1]!r}")
        name, path = argv[i + 1].split("=", 1)
        # NAME becomes sbin/<NAME>. A slash would silently create a
        # subdirectory record that was never declared, and ".." would place
        # the file outside sbin entirely.
        if not name or "/" in name or name in (".", ".."):
            raise InitramfsError(
                f"--extra name {name!r} must be a single path component "
                "(it becomes sbin/<NAME>)"
            )
        if name in extras:
            raise InitramfsError(
                f"--extra {name!r} given twice ({extras[name]!r} then {path!r}); "
                "the second would silently win"
            )
        extras[name] = path
        i += 2
    return ("build", init_path, out_path, extras)


def main(argv):
    try:
        parsed = parse_args(argv)
        if parsed[0] == "verify":
            _, cpio, require = parsed
            blob = read_file(cpio, "cpio archive")
            problems = verify(blob, require)
            if problems:
                for p in problems:
                    print(f"  initramfs: {p}", file=sys.stderr)
                raise InitramfsError(f"{cpio} is not a usable initramfs (see above)")
            names = [n for n, _, _ in parse(blob)]
            print(f"initramfs: verified {len(names)} entries in {cpio} "
                  f"({', '.join(names)})")
            return 0
        _, init_path, out_path, extras = parsed
        blob = build(init_path, extras)
        try:
            with open(out_path, "wb") as f:
                f.write(blob)
        except OSError as e:
            raise InitramfsError(f"cannot write {out_path!r}: {e.strerror}") from None
        print(f"initramfs: {len(blob)} bytes -> {out_path}")
        return 0
    except InitramfsError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
