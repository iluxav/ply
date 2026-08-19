#!/bin/sh
# Cold-path excluded, hot-path benchmark: `ply run` vs `docker run` starting
# a container that immediately exits. Run as root (both tools rootful) for a
# fair comparison:  sudo ./bench/bench.sh [iterations]
set -eu
N="${1:-10}"
HERE=$(cd "$(dirname "$0")/.." && pwd)
HELLO="${PLY_BENCH_IMAGE:-$HERE/bench/hello-0.1.0-linux-x64.img}"

if [ ! -f "$HELLO" ]; then
    echo "building bench image..."
    tmp=$(mktemp -d)
    printf '#include <unistd.h>\nint main(){return 0;}\n' > "$tmp/hello.c"
    if command -v musl-gcc >/dev/null; then musl-gcc -static -Os -o "$tmp/hello" "$tmp/hello.c"
    else cc -static -Os -o "$tmp/hello" "$tmp/hello.c"; fi
    printf '[package]\nname = "hello"\nversion = "0.1.0"\nentrypoint = ["./hello"]\n' > "$tmp/ply.toml"
    ply build "$tmp" -o "$HELLO" >/dev/null
    rm -rf "$tmp"
fi

bench() { # label cmd...
    label="$1"; shift
    # warmup
    "$@" >/dev/null 2>&1 || true
    start=$(date +%s%N)
    i=0
    while [ "$i" -lt "$N" ]; do "$@" >/dev/null 2>&1; i=$((i+1)); done
    end=$(date +%s%N)
    echo "$label: $(( (end - start) / N / 1000000 )) ms/run (avg of $N)"
}

bench "ply run   (thin image) " ply run "$HELLO"

if command -v docker >/dev/null 2>&1; then
    docker pull -q busybox >/dev/null 2>&1 || true
    bench "docker run (busybox)    " docker run --rm busybox true
else
    echo "docker not installed — skipping comparison"
fi
