#!/usr/bin/env python3
"""Tests for scripts/mkinitramfs.py.

mkinitramfs.py writes a binary format by hand, byte by byte, and the kernel
is the only other thing that reads it — so a mistake surfaces as a guest that
does not boot, with no message. These tests are the only automated coverage
that format has.

Run: python3 scripts/test_mkinitramfs.py    (no pytest, no dependencies;
scripts/build-microvm-kernel.sh runs it before it builds anything.)
"""

import os
import subprocess
import sys
import tempfile
import unittest

# No scripts/__pycache__ in a repo that has no other Python package here.
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mkinitramfs as m  # noqa: E402

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "mkinitramfs.py")


def run(*args):
    return subprocess.run(
        [sys.executable, SCRIPT, *args], capture_output=True, text=True
    )


class Newc(unittest.TestCase):
    def test_header_is_exactly_110_bytes(self):
        e = m.entry("x", 0o100755, b"abcd")
        self.assertEqual(e[:6], b"070701")
        self.assertEqual(m.HEADER_LEN, 110)

    def test_field_at_2_32_is_refused_not_widened(self):
        """The bug this guards: f"{f:08X}" emits NINE characters for any
        value >= 2**32, shifting every later record by one byte."""
        with self.assertRaises(m.InitramfsError) as cm:
            m._hex8("filesize", 2**32)
        self.assertIn("8 hex digits", str(cm.exception))
        # and the boundary below it is fine, still 8 wide
        self.assertEqual(m._hex8("filesize", 2**32 - 1), b"FFFFFFFF")
        self.assertEqual(len(m._hex8("ino", 0)), 8)

    def test_negative_field_is_refused(self):
        with self.assertRaises(m.InitramfsError):
            m._hex8("mode", -1)

    def test_roundtrip_preserves_names_modes_and_data(self):
        blob = m.entry("dev", 0o040755)
        blob += m.entry("dev/console", 0o020600, dev=(5, 1))
        blob += m.entry("hello", 0o100755, b"payload")
        blob += m.entry("TRAILER!!!", 0)
        got = m.parse(blob)
        self.assertEqual([n for n, _, _ in got], ["dev", "dev/console", "hello"])
        self.assertEqual(got[1][1]["rdevmajor"], 5)
        self.assertEqual(got[1][1]["rdevminor"], 1)
        self.assertEqual(got[2][2], b"payload")

    def test_records_stay_4_byte_aligned_for_every_name_length(self):
        """The kernel's newc reader pads name and data to 4 bytes; an
        off-by-one here is invisible until the guest fails to boot."""
        for n in range(1, 40):
            blob = m.entry("a" * n, 0o100644, b"z" * (n % 7))
            blob += m.entry("after", 0o100644, b"ok")
            blob += m.entry("TRAILER!!!", 0)
            got = m.parse(blob)
            self.assertEqual([x[0] for x in got], ["a" * n, "after"], f"name len {n}")
            self.assertEqual(got[1][2], b"ok", f"name len {n}")


class BuildAndVerify(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.d = self.tmp.name
        self.init = os.path.join(self.d, "init-bin")
        with open(self.init, "wb") as f:
            f.write(b"\x7fELF" + b"stub" * 16)
        self.addCleanup(self.tmp.cleanup)

    def path(self, name, data=b"tool"):
        p = os.path.join(self.d, name)
        with open(p, "wb") as f:
            f.write(data)
        return p

    def test_build_contains_the_nodes_the_guest_needs(self):
        blob = m.build(self.init, {})
        names = [n for n, _, _ in m.parse(blob)]
        self.assertIn("dev/console", names)
        self.assertIn("dev/null", names)  # ruling R0-4
        self.assertIn("init", names)
        by = {n: v for n, v, _ in m.parse(blob)}
        self.assertEqual(by["dev/null"]["mode"] & 0o170000, 0o020000)
        self.assertEqual(
            (by["dev/null"]["rdevmajor"], by["dev/null"]["rdevminor"]), (1, 3)
        )
        self.assertEqual(
            (by["dev/console"]["rdevmajor"], by["dev/console"]["rdevminor"]), (5, 1)
        )

    def test_verify_accepts_what_build_produces(self):
        blob = m.build(self.init, {"mke2fs": self.path("mke2fs")})
        self.assertEqual(m.verify(blob, ["sbin/mke2fs"]), [])

    def test_verify_notices_a_missing_dev_null(self):
        """R0-4 in one assertion: the archive that cost an hour."""
        blob = m.entry("dev", 0o040755)
        blob += m.entry("dev/console", 0o020600, dev=(5, 1))
        blob += m.entry("init", 0o100755, b"x")
        blob += m.entry("TRAILER!!!", 0)
        self.assertIn("missing dev/null", m.verify(blob))

    def test_verify_notices_a_wrong_device_number(self):
        blob = m.entry("dev", 0o040755)
        blob += m.entry("dev/console", 0o020600, dev=(5, 1))
        blob += m.entry("dev/null", 0o020666, dev=(1, 5))  # wrong minor
        blob += m.entry("init", 0o100755, b"x")
        blob += m.entry("TRAILER!!!", 0)
        problems = m.verify(blob)
        self.assertTrue(any("dev/null" in p and "1:5" in p for p in problems), problems)

    def test_verify_notices_a_regular_file_where_a_node_belongs(self):
        blob = m.entry("dev", 0o040755)
        blob += m.entry("dev/console", 0o020600, dev=(5, 1))
        blob += m.entry("dev/null", 0o100666)  # regular file, not a node
        blob += m.entry("init", 0o100755, b"x")
        blob += m.entry("TRAILER!!!", 0)
        self.assertTrue(
            any("character device" in p for p in m.verify(blob)), m.verify(blob)
        )

    def test_verify_notices_a_non_executable_init(self):
        blob = m.entry("dev", 0o040755)
        blob += m.entry("dev/console", 0o020600, dev=(5, 1))
        blob += m.entry("dev/null", 0o020666, dev=(1, 3))
        blob += m.entry("init", 0o100644, b"x")
        blob += m.entry("TRAILER!!!", 0)
        self.assertTrue(any("not executable" in p for p in m.verify(blob)))

    def test_verify_notices_a_required_extra_that_is_absent(self):
        blob = m.build(self.init, {})
        self.assertIn("missing sbin/mke2fs", m.verify(blob, ["sbin/mke2fs"]))

    def test_verify_rejects_a_truncated_archive(self):
        blob = m.build(self.init, {"mke2fs": self.path("mke2fs", b"x" * 999)})
        self.assertTrue(m.verify(blob[: len(blob) // 2]))


class Cli(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.d = self.tmp.name
        self.init = os.path.join(self.d, "init-bin")
        with open(self.init, "wb") as f:
            f.write(b"stub")
        self.out = os.path.join(self.d, "out.cpio")
        self.tool = os.path.join(self.d, "tool")
        with open(self.tool, "wb") as f:
            f.write(b"tool")
        self.addCleanup(self.tmp.cleanup)

    def test_happy_path_writes_and_verifies(self):
        r = run(self.init, self.out, "--extra", f"mke2fs={self.tool}")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(os.path.exists(self.out))
        v = run("--verify", self.out, "--require", "sbin/mke2fs")
        self.assertEqual(v.returncode, 0, v.stderr)

    def test_extra_name_with_a_slash_is_refused(self):
        r = run(self.init, self.out, "--extra", f"a/b={self.tool}")
        self.assertEqual(r.returncode, 1)
        self.assertIn("single path component", r.stderr)
        self.assertFalse(os.path.exists(self.out))

    def test_extra_name_of_dotdot_is_refused(self):
        r = run(self.init, self.out, "--extra", f"..={self.tool}")
        self.assertEqual(r.returncode, 1)
        self.assertIn("single path component", r.stderr)

    def test_duplicate_extra_name_is_refused(self):
        r = run(self.init, self.out, "--extra", f"mke2fs={self.tool}",
                "--extra", f"mke2fs={self.init}")
        self.assertEqual(r.returncode, 1)
        self.assertIn("given twice", r.stderr)

    def test_missing_file_is_a_message_not_a_traceback(self):
        r = run(os.path.join(self.d, "nope"), self.out)
        self.assertEqual(r.returncode, 1)
        self.assertNotIn("Traceback", r.stderr)
        self.assertIn("cannot read init binary", r.stderr)

    def test_missing_extra_file_is_a_message_not_a_traceback(self):
        r = run(self.init, self.out, "--extra", "mke2fs=/nonexistent/mke2fs")
        self.assertEqual(r.returncode, 1)
        self.assertNotIn("Traceback", r.stderr)
        self.assertIn("--extra mke2fs", r.stderr)

    def test_extra_without_equals_is_a_message(self):
        r = run(self.init, self.out, "--extra", "mke2fs")
        self.assertEqual(r.returncode, 1)
        self.assertIn("NAME=PATH", r.stderr)

    def test_verify_of_a_bad_archive_exits_nonzero(self):
        with open(self.out, "wb") as f:
            f.write(b"not a cpio at all")
        r = run("--verify", self.out)
        self.assertEqual(r.returncode, 1)
        self.assertNotIn("Traceback", r.stderr)

    def test_the_script_is_executable(self):
        self.assertTrue(os.access(SCRIPT, os.X_OK), f"{SCRIPT} is not mode 0755")


if __name__ == "__main__":
    unittest.main(verbosity=2)
