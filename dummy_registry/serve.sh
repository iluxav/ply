#!/bin/sh
# A complete "registry server": any dumb file host works.
set -eu
cd "$(dirname "$0")/registry"
PORT="${1:-8321}"
echo "serving dummy registry at http://127.0.0.1:$PORT"
echo "use in ply.toml:  [sources]  default = \"http://127.0.0.1:$PORT\""
exec python3 -m http.server "$PORT"
