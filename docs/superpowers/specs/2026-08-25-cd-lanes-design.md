# ply CD — three lanes, zero daemons

**Date:** 2026-08-25 (lane 3 + swap + cgroup knobs built same night; lanes 1
was already live; lane 2 + dashboard tab = next session)
**North star check:** no lane adds a resident process. Freshness is
pull-based; auto-deploy (v2) is a systemd timer, not a webhook.

## The deployment spec (one file, three source kinds)

```toml
# /var/lib/ply/deployments/<name>.toml — exactly one of:
app    = "postgres"          # lane 1: ply registry runnable  [LIVE]
github = "org/repo"          # lane 3: CI-built .img on GitHub releases  [BUILT]
repo   = "git@github.com:…"  # lane 2: build on the droplet  [NEXT]

# lane-3 extras
asset      = "dashboard"     # <name> in <name>-<ver>-linux-<arch>.img; default: deployment name
token_file = "/etc/ply/tokens/x"   # fine-grained PAT (private repos). Deploy keys do NOT work for assets.

# lane-2 extras (to build)
ref        = "main"
build      = "npm ci && npm run build"   # runs in a memory-fenced ply container (runtime keg)
runtime    = "node@24"                   # builder dependency from the registry
deploy_key = "/etc/ply/keys/x"           # read-only per-repo SSH key; also covers ls-remote polling
# + generated-manifest fields when the repo has no ply.toml: entrypoint, include

# common
version = "17"        # constraint; lane 3: exact x.y.z pins, else follow latest
publish = ["internal:5432"]
domain  = ["app.example.com"]
env     = { KEY = "value" }
env_file = "/etc/ply/env/x"
scale   = 2
after   = ["db"]
grant_links = false
```

## Lane 3 mechanics (built tonight)

- latest version, public: HEAD `releases/latest/download/probe` → parse the
  redirect Location. No API, no token, no rate limit.
- private: REST API with the PAT (latest + asset id + octet-stream download).
- fetch → sha256 → store.insert → unit → enable/restart. Status records
  `<asset>-<ver>-…img (github:org/repo)`.
- exact `version = "x.y.z"` pins; `"x.y"`/none follows latest (re-resolved on
  every reconcile — the v2 timer makes that continuous CD).

## Lane 2 mechanics (next session)

- checkout persists at /var/lib/ply/builds/<name>/ — THE CHECKOUT IS THE
  CACHE (node_modules + .next/cache survive; first build pays full price,
  every later one is incremental).
- builder = generated throwaway ply app: base debian, dep = `runtime`,
  entrypoint sh -c <build>, workdir /work (checkout bind), [resources]
  mem≈60% RAM + cpu_weight 25 + swap unlimited (knobs exist as of tonight).
- JS guard: small RAM + no swap → refuse with `sudo ply setup --swap 2G`
  hint (built tonight). Per-runtime tempering: NODE_OPTIONS sized to fence,
  CARGO_BUILD_JOBS=1.
- repo ply.toml wins; else generate from spec fields.
- freshness: `git ls-remote` hash vs deployed hash in status.
- RAM truth table (docs): 512MB fine for Python/Go/static; JS needs swap
  (slow) — 1GB+swap sane floor; Rust → use CI lane.

## Framework presets (validated live 2026-08-25 on a 512MB droplet)

The dashboard's "from source" tab prefills per framework; these are the
KNOWN-GOOD commands, not guesses:

- **Next.js** (needs `output: "standalone"` in next.config):
  build = "npm ci && npm run build && rm -rf .next/standalone/.next/static .next/standalone/public && cp -r .next/static .next/standalone/.next/static && cp -r public .next/standalone/public"
  entrypoint = ["node", ".next/standalone/server.js"], include = [".next/standalone/"]
  (standalone output does NOT carry static assets — the copy step is
  mandatory or /_next/static 404s; same dance as ply's own deploy-web.)
- **Node server**: build = "npm ci && npm run build",
  entrypoint = ["node", "dist/index.js"], include = ["dist/", "node_modules/", "package.json"]
- **Static** (build to dist/): serve via caddy keg or a tiny file server — TBD.
- **Go**: build = "go build -o server ." with runtime = "go@…" once a go keg
  ships; until then lane 3 (CI) is the steer.

Builder facts from the live run (Basic 1vCPU/512MB/10GB droplet, WHILE
serving dashboard + redis + the app itself):
- cold build 209s, incremental rebuild 80s (checkout-is-the-cache payoff)
- host peaks during builds (DO insights): CPU 70% for ~1 min, memory 79%
  for ~4 min, swap peak 65MB — the fence (mem 384M + cpu_weight 25) kept
  the machine responsive throughout; running apps never evicted. Compare
  Coolify's docs: builds "could make your server unresponsive".
- overlay is RAM-backed tmpfs so ALL caches must land in /work
  (npm_config_cache, TMPDIR — done); npm cache in the checkout doubles as
  the cross-build cache.

## Dashboard (next session)

Deploy page grows tabs: registry [live] · GitHub releases (repo, asset,
private→token instructions) · from source (repo URL + framework preset
filling build/runtime/entrypoint/include). Freshness line per deployment +
deploy-now button (touches the spec → inotify → reconcile).

## v2: the timer

`ply-reconcile.timer` OnUnitActiveSec=60s → `ply reconcile` re-resolves
follow-latest lanes → continuous deployment, still zero resident processes.
`auto = false` per spec opts out (status shows "update available" instead).
