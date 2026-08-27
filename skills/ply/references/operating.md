# Operating a ply host

Everything is a file. You operate a ply host by reading and writing files
at stable paths — usually over ssh as root. There is no API server, no
token, no SDK. Declare desired state; `ply reconcile` (systemd inotify +
a 1-minute timer) converges reality to it; read the outcome back.

## The file map

Rootful paths (servers). Rootless dev boxes: state/logs under
`$XDG_RUNTIME_DIR/ply/`, apps under `~/.local/share/ply/apps/`.

| path | what | you |
|---|---|---|
| `/var/lib/ply/deployments/<name>.toml` | a deployment (desired state) | write |
| `/var/lib/ply/deployments/.status/<name>.status` | last reconcile verdict, one JSON line `{ok,detail,ts}` | read |
| `/var/lib/ply/deployments/.status/fleet.json` | GitOps sync state (fleet hosts) | read |
| `/var/lib/ply/apps/events.log` | journal: deploys, scales, restarts, crash respawns (JSON lines) | read / `tail -f` |
| `/run/ply/logs/<app>.<n>.log` (+ `.1` rotation) | instance stdout ring — survives the instance | read |
| `/var/lib/ply/apps/<app>/control/scale` | write a number 1..100 | write |
| `/var/lib/ply/apps/<app>/control/restart` | rolling restart (content ignored) | write |
| `/var/lib/ply/apps/<app>/control/last-result` | command outcome, JSON | read |
| `/var/lib/ply/deployments/.keys/` | tokens & deploy keys (0600, root) | write once |

`ply ps --json` lists instances machine-readably. `ply exec <app> sh`
opens a shell inside an instance.

## Deploying: write one file

Exactly one source per spec: `app =` (registry), `image =` (local file),
`github =` (release assets; add `tag_prefix = "web-v"` for monorepo
streams), `repo =` (clone + build on this host). Write atomically
(temp file + `mv`) or with a single redirect:

```sh
cat > /var/lib/ply/deployments/api.toml <<'EOF'
repo = "https://github.com/org/api"
build = "npm ci && npm run build"
runtime = "node@24"
entrypoint = ["node", "dist/index.js"]
port = 3000
publish = ["internal:3000"]
domain = ["api.example.com"]
EOF
```

Then poll `.status/api.status` — expect `building @ <commit>…` then
`deployed`/`rolled …`, or a failure with the reason. Do not retry in a
loop yourself: reconcile re-runs every minute and backs failed builds off
for 10 minutes. The file's mtime is intent: `touch` = deploy now (works
even with `auto = false`); editing = converge to the edit.

## The diagnose loop

1. `cat .status/<name>.status` — the verdict, verbatim.
2. `tail -50 /var/lib/ply/apps/events.log` — what led here (deploy-failed,
   crash respawns with restart counts).
3. For build failures: `cat /run/ply/logs/<name>-builder.1.log` — the
   builder is a real app and its ring **outlives it**; the compiler error
   is in there.
4. For runtime crashes: the app's own ring + `ply ps` (a `*` on status
   means the supervisor predates the installed binary — restart the unit).
5. Fix the SPEC (or the code), never the generated systemd unit — units
   headed `managed by ply reconcile` are overwritten on every converge.

Rollback = pin the spec: `version = "1.4.2"` (registry/github lanes) or
`ref = "<commit>"` (repo lane). Remove the pin to follow latest again.

## Cautions

- Secrets never go in the spec if avoidable: use `env_file = "/root/x.env"`
  or `token_file = ".keys/<name>.token"` (relative = under the
  deployments dir; you create the key file, 0600).
- One deployment per app name — two specs resolving to the same inner app
  name will be refused.
- Never write into `.status/` (reconcile owns it) and never create scratch
  files in the deployments dir root — the directory is inotify-watched and
  every file event triggers a reconcile run.
- On a GitOps fleet host (`.status/fleet.json` exists): git owns the specs
  it introduced — edit the infra repo (or open a PR), not the synced file;
  your direct edit is overwritten on the next beat. Locally-created specs
  coexist and stay yours.
- `grant_links = true` is required by apps whose manifest `[requests]`
  links (the dashboard, notify) — without it they run blind, not broken.
