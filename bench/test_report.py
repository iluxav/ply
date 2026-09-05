"""report.py is the judgment: which cells are findings. Test that, not the
formatting."""
import unittest

import report


def cell(runtime, rps=1000.0, p99=5.0, cpu=2.0, phase="steady", path="published-lan",
         endpoint="ping", mode="", errors=0):
    return {"phase": phase, "runtime": runtime, "path": path, "endpoint": endpoint,
            "mode": mode, "conc": 64, "secs": 15, "rps": rps, "p50_ms": p99 / 3,
            "p90_ms": p99 / 1.5, "p99_ms": p99, "errors": errors, "rt_cpu_s": cpu,
            "rt_rss_peak_kb": 30000}


class Flags(unittest.TestCase):
    def test_within_noise_is_not_a_finding(self):
        self.assertEqual(report.flags(cell("ply", rps=950, p99=5.5, cpu=2.2), cell("docker")), [])

    def test_each_threshold_is_named(self):
        got = report.flags(cell("ply", rps=700, p99=7.0, cpu=3.0), cell("docker"))
        self.assertEqual(got, ["rps 0.70x", "p99 1.40x", "runtime cpu 1.50x"])

    def test_errors_are_always_a_finding(self):
        self.assertEqual(report.flags(cell("ply", errors=3), cell("docker")), ["3 errors"])

    def test_a_cell_with_no_result_is_a_finding(self):
        self.assertEqual(report.flags(cell("ply", rps=0, p99=0, cpu=0, errors=-1), cell("docker")), ["no result"])

    def test_a_zero_reference_cannot_divide(self):
        self.assertEqual(report.flags(cell("ply", cpu=1.0), cell("docker", cpu=0.0)), [])


class Pairing(unittest.TestCase):
    def test_ply_cell_pairs_with_docker_on_the_same_key(self):
        cells = [cell("ply"), cell("docker"), cell("ply", endpoint="read"), cell("docker", endpoint="read")]
        pairs = report.pair(cells)
        self.assertEqual(sorted(p.ply["endpoint"] for p in pairs), ["ping", "read"])
        self.assertTrue(all(p.ref["runtime"] == "docker" for p in pairs))

    def test_ply_only_variant_pairs_with_plys_own_base(self):
        base = cell("ply"); audit = cell("ply", mode="egress-audit", rps=900)
        pairs = report.pair([base, audit, cell("docker")])
        variant = [p for p in pairs if p.ply["mode"] == "egress-audit"][0]
        self.assertIs(variant.ref, base)
        self.assertEqual(variant.ref_label, "ply base")

    def test_a_cell_with_no_reference_stands_alone(self):
        pairs = report.pair([cell("ply", phase="churn", mode="rolling-deploy")])
        self.assertIsNone(pairs[0].ref)


class Soak(unittest.TestCase):
    def test_flat_series_has_zero_slope(self):
        self.assertAlmostEqual(report.slope_per_min([(0, 100), (60, 100), (120, 100)]), 0.0)

    def test_slope_is_per_minute(self):
        # +2048 kB over 120 s = 1024 kB/min
        self.assertAlmostEqual(report.slope_per_min([(0, 1000), (60, 2024), (120, 3048)]), 1024.0)

    def test_drift_flags_rss_over_1mib_per_min_and_fds_over_1_per_min(self):
        rows = [{"ts": t, "runtime": "ply", "cell": "soak", "rt_rss_kb": 10000 + 20 * t,
                 "rt_fds": 50 + t / 30, "rt_cpu_s": 1} for t in range(0, 600, 2)]
        f = report.soak_findings(rows)
        self.assertGreater(f["rss_slope_kb_min"], 1024)
        self.assertGreater(f["fd_slope_min"], 1)
        self.assertEqual(f["flags"], ["rss +1200 kB/min", "fds +2.0/min"])

    def test_steady_soak_has_no_flags(self):
        rows = [{"ts": t, "runtime": "docker", "cell": "soak", "rt_rss_kb": 10000, "rt_fds": 50,
                 "rt_cpu_s": 1} for t in range(0, 600, 2)]
        self.assertEqual(report.soak_findings(rows)["flags"], [])


class Reference(unittest.TestCase):
    def test_docker_cells_are_borrowed_from_a_reference_run_when_this_run_has_none(self):
        cells = report.with_reference([cell("ply")], [cell("docker"), cell("ply", rps=500)])
        runtimes = sorted(c["runtime"] for c in cells)
        self.assertEqual(runtimes, ["docker", "ply"])
        self.assertTrue(all(c.get("borrowed") for c in cells if c["runtime"] == "docker"))

    def test_a_run_with_its_own_docker_cells_keeps_them(self):
        own = cell("docker", rps=999)
        cells = report.with_reference([cell("ply"), own], [cell("docker", rps=1)])
        docker = [c for c in cells if c["runtime"] == "docker"]
        self.assertEqual([c["rps"] for c in docker], [999])

    def test_previous_ply_run_renders_a_before_after_table(self):
        now = [cell("ply", rps=900, cpu=1.0), cell("docker")]
        prev = [cell("ply", rps=600, cpu=45.0)]
        md = report.render(now, [], prev=prev)
        self.assertIn("## ply before → after", md)
        section = md.split("## ply before → after")[1]
        self.assertIn("600", section)
        self.assertIn("900", section)
        self.assertIn("1.50x", section)
        self.assertIn("45.0", section)


class Render(unittest.TestCase):
    def test_a_run_without_ply_cells_says_so_instead_of_claiming_a_pass(self):
        md = report.render([cell("docker")], [])
        self.assertIn("No ply cells", md)
        self.assertNotIn("every ply cell", md)

    def test_findings_section_lists_only_flagged_cells(self):
        cells = [cell("ply"), cell("docker"), cell("ply", endpoint="write", rps=400),
                 cell("docker", endpoint="write")]
        md = report.render(cells, [])
        self.assertIn("## Findings", md)
        self.assertIn("write", md.split("## Findings")[1])
        self.assertIn("rps 0.40x", md)
        self.assertNotIn("| ping", md.split("## Findings")[1])


if __name__ == "__main__":
    unittest.main()
