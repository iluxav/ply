#!/usr/bin/env python3
"""Turn bench/results/<stamp>/{cells.jsonl,samples.csv} into report.md.

Docker is the reference. A ply cell is a FINDING when it is clearly worse
than its reference (rps < 0.8x, p99 > 1.25x, runtime CPU > 1.25x, or any
errors); a ply-only variant (egress modes, DB-direct) is judged against
ply's own base cell; the soak is judged by the slope of the runtime's RSS
and FD count over time. Everything else is one table line.
"""
import csv
import json
import sys
from collections import namedtuple
from pathlib import Path

RPS_FLOOR = 0.8
P99_CEIL = 1.25
CPU_CEIL = 1.25
RSS_SLOPE_KB_MIN = 1024.0
FD_SLOPE_MIN = 1.0

Pair = namedtuple("Pair", "ply ref ref_label")


def key(c):
    return (c["phase"], c["path"], c["endpoint"], c["mode"], c["conc"])


def base_key(c):
    return (c["phase"], c["path"], c["endpoint"], "", c["conc"])


def flags(ply, ref):
    if ply.get("errors", 0) < 0:
        return ["no result"]
    out = []
    if ref.get("rps") and ply["rps"] / ref["rps"] < RPS_FLOOR:
        out.append("rps %.2fx" % (ply["rps"] / ref["rps"]))
    if ref.get("p99_ms") and ply["p99_ms"] / ref["p99_ms"] > P99_CEIL:
        out.append("p99 %.2fx" % (ply["p99_ms"] / ref["p99_ms"]))
    if ref.get("rt_cpu_s") and ply["rt_cpu_s"] / ref["rt_cpu_s"] > CPU_CEIL:
        out.append("runtime cpu %.2fx" % (ply["rt_cpu_s"] / ref["rt_cpu_s"]))
    if ply.get("errors"):
        out.append("%d errors" % ply["errors"])
    return out


def pair(cells):
    docker = {key(c): c for c in cells if c["runtime"] == "docker"}
    ply_base = {key(c): c for c in cells if c["runtime"] == "ply" and c["mode"] == ""}
    pairs = []
    for c in cells:
        if c["runtime"] != "ply":
            continue
        if key(c) in docker:
            pairs.append(Pair(c, docker[key(c)], "docker"))
        elif c["mode"] and base_key(c) in ply_base:
            pairs.append(Pair(c, ply_base[base_key(c)], "ply base"))
        else:
            pairs.append(Pair(c, None, None))
    return pairs


def slope_per_min(points):
    """Least-squares slope of value over seconds, scaled to per minute."""
    n = len(points)
    if n < 2:
        return 0.0
    sx = sum(p[0] for p in points)
    sy = sum(p[1] for p in points)
    sxx = sum(p[0] * p[0] for p in points)
    sxy = sum(p[0] * p[1] for p in points)
    den = n * sxx - sx * sx
    if den == 0:
        return 0.0
    return (n * sxy - sx * sy) / den * 60.0


def soak_findings(rows):
    rows = sorted(rows, key=lambda r: float(r["ts"]))
    t0 = float(rows[0]["ts"]) if rows else 0.0
    rss = [(float(r["ts"]) - t0, float(r["rt_rss_kb"])) for r in rows]
    fds = [(float(r["ts"]) - t0, float(r["rt_fds"])) for r in rows]
    f = {"rss_slope_kb_min": slope_per_min(rss), "fd_slope_min": slope_per_min(fds), "flags": []}
    if f["rss_slope_kb_min"] > RSS_SLOPE_KB_MIN:
        f["flags"].append("rss %+.0f kB/min" % f["rss_slope_kb_min"])
    if f["fd_slope_min"] > FD_SLOPE_MIN:
        f["flags"].append("fds %+.1f/min" % f["fd_slope_min"])
    if rows:
        f["rss_first_kb"] = float(rows[0]["rt_rss_kb"])
        f["rss_last_kb"] = float(rows[-1]["rt_rss_kb"])
        f["fds_first"] = float(rows[0]["rt_fds"])
        f["fds_last"] = float(rows[-1]["rt_fds"])
        f["cpu_s"] = float(rows[-1]["rt_cpu_s"]) - float(rows[0]["rt_cpu_s"])
        f["secs"] = float(rows[-1]["ts"]) - t0
    return f


def with_reference(cells, ref_cells):
    """Borrow the reference run's Docker cells when this run has none of its
    own (RUNTIMES=ply): same box, an hour apart, is a fair enough reference
    and saves the 15 minutes. Borrowed cells are marked."""
    if any(c["runtime"] == "docker" for c in cells):
        return list(cells)
    borrowed = [dict(c, borrowed=True) for c in ref_cells if c["runtime"] == "docker"]
    return list(cells) + borrowed


def ratio(a, b):
    return "%.2fx" % (a / b) if b else "-"


def render(cells, samples, prev=None):
    pairs = pair(cells)
    out = ["# ply vs Docker under load", ""]
    if any(c.get("borrowed") for c in cells):
        out += ["Docker cells are borrowed from a reference run (same box, earlier today).", ""]
    findings = []
    for phase in ("steady", "churn", "soak"):
        rows = [p for p in pairs if p.ply["phase"] == phase]
        if not rows:
            continue
        out += ["## %s" % phase.capitalize(), "",
                "| path | endpoint | mode | ply rps | ref rps | ratio | ply p50/p99 ms | ref p50/p99 ms | p99 ratio | ply rt cpu s | ref rt cpu s | cpu ratio | errors | ref |",
                "|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|"]
        for p in rows:
            c, r = p.ply, p.ref
            fl = flags(c, r) if r else (["%d errors" % c["errors"]] if c.get("errors") else [])
            if fl:
                findings.append((c, p.ref_label, fl))
            mark = " **!**" if fl else ""
            out.append("| %s | %s | %s | %.0f | %s | %s | %.1f/%.1f | %s | %s | %.1f | %s | %s | %d | %s%s |" % (
                c["path"], c["endpoint"], c["mode"] or "base", c["rps"],
                "%.0f" % r["rps"] if r else "-", ratio(c["rps"], r["rps"]) if r else "-",
                c["p50_ms"], c["p99_ms"],
                "%.1f/%.1f" % (r["p50_ms"], r["p99_ms"]) if r else "-",
                ratio(c["p99_ms"], r["p99_ms"]) if r else "-",
                c["rt_cpu_s"], "%.1f" % r["rt_cpu_s"] if r else "-",
                ratio(c["rt_cpu_s"], r["rt_cpu_s"]) if r else "-",
                c["errors"], p.ref_label or "none", mark))
        out.append("")
    docker_cells = [c for c in cells if c["runtime"] == "docker"]
    if docker_cells:
        out += ["## Docker reference cells", "", "| phase | path | endpoint | mode | rps | p50/p99 ms | rt cpu s | errors |", "|---|---|---|---|---:|---:|---:|---:|"]
        for c in docker_cells:
            out.append("| %s | %s | %s | %s | %.0f | %.1f/%.1f | %.1f | %d |" % (
                c["phase"], c["path"], c["endpoint"], c["mode"] or "base", c["rps"], c["p50_ms"], c["p99_ms"], c["rt_cpu_s"], c["errors"]))
        out.append("")
    soak = {}
    for rt in ("ply", "docker"):
        rows = [s for s in samples if s["runtime"] == rt and s["cell"].startswith("soak")]
        if rows:
            soak[rt] = soak_findings(rows)
    if soak:
        out += ["## Soak drift", "", "| runtime | seconds | rt rss first → last kB | slope kB/min | fds first → last | slope /min | rt cpu s |", "|---|---:|---:|---:|---:|---:|---:|"]
        for rt, f in soak.items():
            out.append("| %s | %.0f | %.0f → %.0f | %+.0f | %.0f → %.0f | %+.2f | %.1f |" % (
                rt, f["secs"], f["rss_first_kb"], f["rss_last_kb"], f["rss_slope_kb_min"],
                f["fds_first"], f["fds_last"], f["fd_slope_min"], f["cpu_s"]))
            if f["flags"]:
                findings.append(({"phase": "soak", "path": rt, "endpoint": "-", "mode": "drift"}, "time", f["flags"]))
        out.append("")
    if prev:
        prev_ply = {key(c): c for c in prev if c["runtime"] == "ply"}
        rows = [(c, prev_ply[key(c)]) for c in cells if c["runtime"] == "ply" and key(c) in prev_ply]
        if rows:
            out += ["## ply before → after", "",
                    "| phase | path | endpoint | mode | rps before → after | ratio | p99 ms before → after | rt cpu s before → after | errors before → after |",
                    "|---|---|---|---|---:|---:|---:|---:|---:|"]
            for c, p in rows:
                out.append("| %s | %s | %s | %s | %.0f → %.0f | %s | %.2f → %.2f | %.1f → %.1f | %d → %d |" % (
                    c["phase"], c["path"], c["endpoint"], c["mode"] or "base", p["rps"], c["rps"],
                    ratio(c["rps"], p["rps"]), p["p99_ms"], c["p99_ms"], p["rt_cpu_s"], c["rt_cpu_s"],
                    p["errors"], c["errors"]))
            out.append("")
    out += ["## Findings", ""]
    if not any(c["runtime"] == "ply" for c in cells):
        out.append("No ply cells in this run (RUNTIMES did not include ply).")
    elif not findings:
        out.append("None: every ply cell is within the thresholds of its reference, and nothing drifted over the soak.")
    for c, ref, fl in findings:
        out.append("- **%s / %s / %s / %s** vs %s: %s" % (c["phase"], c["path"], c["endpoint"], c["mode"] or "base", ref, ", ".join(fl)))
    out.append("")
    out += ["Thresholds: rps < %.2fx, p99 > %.2fx, runtime CPU > %.2fx of the reference, any error; soak RSS slope > %.0f kB/min or FD slope > %.0f/min." % (RPS_FLOOR, P99_CEIL, CPU_CEIL, RSS_SLOPE_KB_MIN, FD_SLOPE_MIN), ""]
    return "\n".join(out)


def load(results_dir):
    d = Path(results_dir)
    cells = [json.loads(l) for l in (d / "cells.jsonl").read_text().splitlines() if l.strip()]
    samples = []
    p = d / "samples.csv"
    if p.exists():
        with p.open() as fh:
            samples = list(csv.DictReader(fh))
    return cells, samples


if __name__ == "__main__":
    import argparse
    ap = argparse.ArgumentParser()
    ap.add_argument("results")
    ap.add_argument("--ref", help="results dir whose Docker cells stand in when this run has none")
    ap.add_argument("--prev", help="an earlier run of ply to show before → after")
    a = ap.parse_args()
    cells, samples = load(a.results)
    prev = None
    if a.ref:
        ref_cells, ref_samples = load(a.ref)
        cells = with_reference(cells, ref_cells)
        if not any(s["runtime"] == "docker" for s in samples):
            samples = samples + [s for s in ref_samples if s["runtime"] == "docker"]
    if a.prev:
        prev, _ = load(a.prev)
    md = render(cells, samples, prev=prev)
    try:
        (Path(a.results) / "report.md").write_text(md)
    except OSError as e:
        print("(report.md not written: %s)" % e, file=sys.stderr)
    print(md)
