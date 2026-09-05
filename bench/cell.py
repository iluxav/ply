#!/usr/bin/env python3
"""oha JSON result(s) + this cell's sampler rows -> one line of cells.jsonl.

Several --oha files are one cell measured in segments (the soak): requests
and errors add up, rps is the total over the total time, the latency
percentiles are the worst segment's, and `segments` keeps each segment's
rps so drift in throughput is visible next to drift in memory.
"""
import argparse
import csv
import json
import sys


def errors_in(d):
    # Requests still in flight when the clock ran out are not failures.
    e = sum(n for k, n in d.get("errorDistribution", {}).items() if k != "aborted due to deadline")
    e += sum(n for code, n in d.get("statusCodeDistribution", {}).items() if not str(code).startswith("2"))
    return e


def aggregate(results):
    requests = sum(sum(d.get("statusCodeDistribution", {}).values()) for d in results)
    secs = sum(d["summary"]["total"] for d in results)
    return {
        "rps": round(requests / secs, 1) if secs else 0.0,
        "p50_ms": round(max(d["latencyPercentiles"]["p50"] for d in results) * 1000, 3),
        "p90_ms": round(max(d["latencyPercentiles"]["p90"] for d in results) * 1000, 3),
        "p99_ms": round(max(d["latencyPercentiles"]["p99"] for d in results) * 1000, 3),
        "errors": sum(errors_in(d) for d in results),
        "requests": requests,
        "segments": [round(d["summary"]["requestsPerSec"], 1) for d in results],
    }


def runtime_stats(samples_path, cell_name):
    cpu_first = cpu_last = None
    rss_peak = 0
    try:
        for row in csv.DictReader(open(samples_path)):
            if row["cell"] != cell_name:
                continue
            c = float(row["rt_cpu_s"])
            cpu_first = c if cpu_first is None else cpu_first
            cpu_last = c
            rss_peak = max(rss_peak, int(row["rt_rss_kb"]))
    except FileNotFoundError:
        pass
    return round(cpu_last - cpu_first, 2) if cpu_first is not None else 0.0, rss_peak


def main():
    ap = argparse.ArgumentParser()
    for f in ("phase", "runtime", "path", "endpoint", "mode", "cell", "samples"):
        ap.add_argument("--" + f, required=True)
    ap.add_argument("--oha", required=True, nargs="+")
    ap.add_argument("--conc", type=int, required=True)
    ap.add_argument("--secs", type=int, required=True)
    a = ap.parse_args()

    base = {"phase": a.phase, "runtime": a.runtime, "path": a.path, "endpoint": a.endpoint,
            "mode": a.mode, "conc": a.conc, "secs": a.secs}
    results = []
    bad = []
    for path in a.oha:
        try:
            d = json.load(open(path))
            d["latencyPercentiles"]
            results.append(d)
        except (OSError, ValueError, KeyError) as e:
            bad.append("%s: %s" % (path, e))
    if not results:
        # oha died or wrote nothing: the cell exists, says so, and the run goes on.
        base.update({"rps": 0.0, "p50_ms": 0.0, "p90_ms": 0.0, "p99_ms": 0.0, "errors": -1,
                     "requests": 0, "rt_cpu_s": 0.0, "rt_rss_peak_kb": 0,
                     "note": "no oha result: " + "; ".join(bad)})
        print(json.dumps(base))
        return
    base.update(aggregate(results))
    cpu, rss = runtime_stats(a.samples, a.cell)
    base["rt_cpu_s"] = cpu
    base["rt_rss_peak_kb"] = rss
    if bad:
        base["note"] = "missing segments: " + "; ".join(bad)
    print(json.dumps(base))


if __name__ == "__main__":
    main()
