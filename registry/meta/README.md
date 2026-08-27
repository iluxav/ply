# Registry metadata — curated, code-reviewed

One JSON file per package: `meta/<owner>/<name>.json`. Merged into the
published `state.json` by `scripts/registry-push.mjs` — the catalog is
whatever this tree says, and changing it is a PR, not a dashboard release.

Fields (all optional):
- `type` — "app" (installable from the dashboard) | "layer" (composition
  keg) | "stack" (a published composition). Default: apps/ namespace →
  app, ply/ namespace → layer.
- `description`, `license`, `homepage`
- `contract` — env lines the dashboard prefills on deploy (apps)
- `publish` — suggested `--publish` (apps)
- `grant_links` — the app needs its `[requests]` links granted
- `origin` — phantom packages: where the artifact actually lives
  (`github:org/repo`, `docker://image`, `https://…`) — the registry
  carries metadata + digests, not bytes
