#!/bin/sh
# Compile Tailwind for both pages → web/dist/styles.css
set -eu
cd "$(dirname "$0")"
[ -d node_modules ] || npm install
npx @tailwindcss/cli -i input.css -o dist/styles.css --minify
echo "built dist/styles.css ($(wc -c < dist/styles.css) bytes)"
