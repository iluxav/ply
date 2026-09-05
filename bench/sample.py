#!/usr/bin/env python3
"""Sample the RUNTIME's own processes every INTERVAL seconds into samples.csv.

ply: every process whose comm is `ply` (the run parents; instances are the
app's own binaries). Docker: dockerd, containerd, containerd-shim*,
docker-proxy. Plus the app's and DB's RSS by pid, and nf_conntrack_count.
Runs until SIGTERM. FD counts need root for other users' processes; -1 when
unreadable.
"""
import argparse
import csv
import os
import signal
import sys
import time

CLK_TCK = os.sysconf("SC_CLK_TCK")
DOCKER_COMMS = {"dockerd", "containerd", "docker-proxy"}


def runtime_pids(runtime):
    pids = []
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            with open(f"/proc/{d}/comm") as fh:
                comm = fh.read().strip()
        except OSError:
            continue
        if runtime == "ply" and comm == "ply":
            pids.append(int(d))
        elif runtime == "docker" and (comm in DOCKER_COMMS or comm.startswith("containerd-shim")):
            pids.append(int(d))
    return pids


def proc_stats(pid):
    """(rss_kb, cpu_seconds, fds, threads) or None if the pid is gone."""
    try:
        with open(f"/proc/{pid}/stat") as fh:
            fields = fh.read().rsplit(")", 1)[1].split()
        # after the comm: state is fields[0]; utime is index 11, stime 12,
        # num_threads 17 (0-based from 'state')
        cpu = (int(fields[11]) + int(fields[12])) / CLK_TCK
        threads = int(fields[17])
        rss_kb = 0
        with open(f"/proc/{pid}/status") as fh:
            for line in fh:
                if line.startswith("VmRSS:"):
                    rss_kb = int(line.split()[1])
                    break
        try:
            fds = len(os.listdir(f"/proc/{pid}/fd"))
        except PermissionError:
            fds = -1
        return rss_kb, cpu, fds, threads
    except (OSError, IndexError, ValueError):
        return None


def rss_kb(pid):
    if pid <= 0:
        return 0
    s = proc_stats(pid)
    return s[0] if s else 0


def conntrack():
    try:
        with open("/proc/sys/net/netfilter/nf_conntrack_count") as fh:
            return int(fh.read())
    except OSError:
        return -1


FIELDS = ["ts", "runtime", "cell", "rt_pids", "rt_rss_kb", "rt_cpu_s", "rt_fds", "rt_threads",
          "app_rss_kb", "db_rss_kb", "conntrack"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runtime", required=True, choices=["ply", "docker"])
    ap.add_argument("--cell", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--app-pid", type=int, default=0)
    ap.add_argument("--db-pid", type=int, default=0)
    ap.add_argument("--interval", type=float, default=2.0)
    a = ap.parse_args()

    stop = False

    def on_term(*_):
        nonlocal stop
        stop = True

    signal.signal(signal.SIGTERM, on_term)
    signal.signal(signal.SIGINT, on_term)

    new = not os.path.exists(a.out) or os.path.getsize(a.out) == 0
    with open(a.out, "a", newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=FIELDS)
        if new:
            w.writeheader()
        while not stop:
            rss = cpu = fds = thr = 0
            n = 0
            unreadable = False
            for pid in runtime_pids(a.runtime):
                s = proc_stats(pid)
                if not s:
                    continue
                n += 1
                rss += s[0]
                cpu += s[1]
                if s[2] < 0:
                    unreadable = True
                else:
                    fds += s[2]
                thr += s[3]
            w.writerow({"ts": f"{time.time():.3f}", "runtime": a.runtime, "cell": a.cell,
                        "rt_pids": n, "rt_rss_kb": rss, "rt_cpu_s": f"{cpu:.2f}",
                        "rt_fds": -1 if unreadable else fds, "rt_threads": thr,
                        "app_rss_kb": rss_kb(a.app_pid), "db_rss_kb": rss_kb(a.db_pid),
                        "conntrack": conntrack()})
            fh.flush()
            t = 0.0
            while t < a.interval and not stop:
                time.sleep(0.1)
                t += 0.1


if __name__ == "__main__":
    main()
