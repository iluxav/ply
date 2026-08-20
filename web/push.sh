#!/bin/sh
# Build and publish the websites:
#   landing  → bucket ply-web       (plybox.sh)
#   registry → bucket ply-registry  (registry.plybox.sh)
# Also ships install.sh so `curl -fsSL https://plybox.sh/install.sh | sh` works.
set -eu
cd "$(dirname "$0")"

./build.sh
node render-registry.mjs   # bake the live state.json into the registry page

put() { # put <bucket/key> <file> <content-type>
    npx wrangler r2 object put "$1" --file "$2" --remote \
        --cache-control "public, max-age=300" --content-type "$3"
}

put ply-web/index.html      landing/index.html   text/html
put ply-web/styles.css      dist/styles.css      text/css
put ply-web/logo.svg        logo.svg             image/svg+xml
put ply-web/install.sh      ../install.sh        text/x-shellscript

put ply-registry/index.html dist/registry-index.html  text/html
put ply-registry/styles.css dist/styles.css      text/css
put ply-registry/logo.svg   logo.svg             image/svg+xml

echo "pushed: plybox.sh (landing + install.sh), registry.plybox.sh (registry page)"
echo "note: state.json is published by scripts/registry-push.mjs (--state-only to force)"
