#!/bin/sh
# Local preview of both sites (each page uses absolute paths like /styles.css,
# so each gets its own port):
#   landing  → http://127.0.0.1:8180
#   registry → http://127.0.0.1:8181
set -eu
cd "$(dirname "$0")"

./build.sh
node render-registry.mjs

rm -rf .preview
mkdir -p .preview/landing .preview/registry

cp landing/index.html dist/styles.css logo.svg ../install.sh .preview/landing/
cp dist/registry-index.html .preview/registry/index.html
cp dist/styles.css logo.svg .preview/registry/
curl -fsS https://registry.plybox.sh/state.json -o .preview/registry/state.json \
    || echo '{}' > .preview/registry/state.json

trap 'kill 0' INT TERM EXIT
python3 -m http.server 8180 -d .preview/landing --bind 127.0.0.1 >/dev/null 2>&1 &
python3 -m http.server 8181 -d .preview/registry --bind 127.0.0.1 >/dev/null 2>&1 &
echo "landing:  http://127.0.0.1:8180"
echo "registry: http://127.0.0.1:8181"
echo "(ctrl-c to stop)"
wait
