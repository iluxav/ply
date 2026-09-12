# Deployment model, simplified: recipe vs order

Status: agreed design, 2026-09-11. Phase 1 (core) implemented and validated
end-to-end on 2026-09-11. This is the north star for the `ply ui` deploy flow
and the reconcile/stack code it sits on.

## Phase 1 — DONE (commits on `deploy-model-simplify`)

- **1b** — `MemberSource::Repo`: a `run = "git+https://…"` member the host
  clones and builds (carrying `build`/`runtime`/`ref`), expanded into a
  `repo=` spec that flows through the existing `build_from_repo`.
- **1a** — a `repo=` deployment whose **ply.toml** is a composition
  (`[[service]]`/`[[app]]`) converges the whole set via `converge_stack`,
  building each member on the box. Reads **ply.toml only** (`stack::load`,
  not `discover`): an app repo that merely *ships* a `stack.toml` (a `ply up`
  convenience) still deploys as its app — topology lives in the recipe, so a
  repo opts into multi-service by putting `[[service]]` in its `ply.toml`.

Validated on a VM: a composition repo (`ply.toml` with a prebuilt `redis`
member + a `git+file://` build member) deployed via one `repo = <path>`
order — the host cloned it, fetched the prebuilt member, **built the git+
member on the box**, wrote both systemd units, and wired them with `after`
(`After=ply-ct-cache.service`, `--after ct-cache`). A plain app repo
(rm-web, whose ply.toml is a single `web` app) still deploys as one app.

## The problem

There are three file-ish things today — `ply.toml`, `stack.toml`, and the
host deployment spec — with overlapping fields (both a stack member and a
deployment spec carry `publish`/`env`). Nobody, including the author, can say
crisply which file does what. That confusion is why the `ply ui` deploy flow
is wrong: pasting the rm-web GitHub URL deployed only `web`, silently
ignoring its `db`/`server`.

## The model: two concepts, separated by owner and time

Not by contents — by **who writes them and when**.

### 1. `ply.toml` — the **recipe** (developer owns it, lives in the repo)

One file format, two shapes. You know which shape you wrote by which table
is present:

- **App shape** — `[package]`. "I am one service." Build instructions
  (`include`, `[dependencies]`, `base`) plus run config (`entrypoint`,
  `[ports]`, `[env]`, volumes, `[health]`, `[restart]`, `[scale]`,
  `[resources]`). `ply build` runs the recipe → one `.img`.

- **Composition shape** — `[[service]]` (today spelled `[[app]]`). "I am
  several services wired together." Each member points at one source, and a
  member's source is one of:
  - **prebuilt** — a registry ref (`postgres@17`), a URL, a `docker://`
    image, or an `.img` path. Nothing to build.
  - **build-from-source** — a repo (`repo = "github.com/..."`) or a local
    dir (`build = "."`). The host/laptop builds it before running.

  Members wire together with `after` (dependency edge + injected
  `<NAME>_ADDR`/`_HOST`/`_PORT` env), and carry per-member `publish`, `env`,
  `domain`, `scale`, `egress`.

**"A stack" stops being a file type.** It is just a `ply.toml` that has
`[[service]]` in it. There is no separate `stack.toml` in the model we teach.
(The engine already accepts `ply.toml` as a valid stack file, so this is
mostly naming/docs, not a rewrite. The `stack.toml` filename may keep working
as a legacy alias; we stop *teaching* it.)

### 2. The deployment — the **order** (operator owns it, lives on the host)

`/var/lib/ply/deployments/<name>.toml`. This is what the `ply ui` form
produces. It **never contains topology**. It says two things:

- **Source** — where the thing comes from, exactly one of:
  - `image = "..."` — a built artifact (registry `ns/name`, URL, or file
    path). Run it, no build.
  - `app = "ns/name"` — a published app/composition from the registry.
  - `repo = "github.com/..."` — source to clone and **build on the host**,
    then run. Honors the repo's `ply.toml` shape: an app builds one service;
    a composition builds/runs all its members.
  - (`github =`, `stack =` remain as today.)
- **Overrides** — `publish`, `env`, `env_file`, `scale`, `domain`. The
  operator's per-host knobs. These are the *only* place per-host values live.

The "image pointer" the user kept reaching for is not a third file — it is
just the string value of `image =` inside the order.

## The load-bearing rule

**Topology lives in the recipe, never inline in the order.** Because topology
is a property of the app (version-controlled, reused across hosts), while
overrides (`publish`/`env`/`domain`/`scale`) are genuinely per-host. An order
references a multi-service app by `repo =` (build it) or `app =`/`stack =`
(pull a published one) — it does not re-declare the services.

Consequence: **publishing to the registry becomes optional.** A 1–5-server
user points `repo =` at their code and the host builds the whole
composition. Publishing is a speed/reuse optimization, not a precondition for
deploying a multi-service app.

## Dev overrides stay exactly as they are

`ply.dev.toml` (app) and `stack.dev.toml` (composition) are **local-only**
overlays applied by `ply run .` / `ply up`, and **never** read by a host
reconciling a deployment. They carry the local truths (a dev password, a
published port that must dodge whatever the laptop already runs) so the
committed recipe never has to hold a dev-shaped lie. This is the reason the
recipe stays publishable, and it does not change. Merge-by-key for
`env`/`params`, replace for `publish`/`run`; an overlay may only override an
existing member, never add one.

## The rm-web walkthrough (the payoff)

rm-web's `ply.toml` is the **composition**: `db = postgres@17` (prebuilt),
`server = repo github.com/iluxav/rm-server` (build), `web = build "."`
(build).

1. In `ply ui` you paste `github.com/iluxav/rm-web`. The order is just
   `repo = "github.com/iluxav/rm-web"` plus your `publish`/`env` overrides.
2. The host clones it, sees `[[service]]`, **builds `server` and `web` on the
   box** (memory-fenced builder + swap), pulls `postgres@17`, wires them with
   `after`, and runs all three.
3. One paste, whole app. No "publish first" tax.

## What actually has to change (why rm-web fails today)

The wall: `Spec::from_stack_member` **rejects a `MemberSource::Path`
(and `Docker`) on a host** — "a local dir is a `ply up` dev thing." A host
composition can therefore only reference already-built registry refs/URLs.
That is exactly why the todos/rm-web stack cannot deploy without publishing
`server` and `web` first.

To make the model real:

1. **Naming/docs (cheap):** teach `ply.toml` + `[[service]]`; stop teaching
   `stack.toml` as its own file. Keep `[[app]]` working as an accepted alias.
2. **A build-from-source member source (core):** add a repo/dir member kind
   that a host can build. Concretely, a new `MemberSource` variant (e.g.
   `Repo`/`Build`) that `from_stack_member` expands into a `repo =` spec
   instead of rejecting.
3. **Host builds a composition from `repo =` (core):** when a `repo =`
   deployment's cloned tree is a composition, reconcile builds each
   build-member on the host and runs the whole set — instead of building only
   the root app.
4. **Per-service overrides in the order (later, small):** a way for an order
   to target one member (e.g. publish only `web`) — a `[service.web]`
   override table. Not needed for the first cut; do not let it block.

## Phasing

- **Phase 1 — core: DONE + VM-validated.** `MemberSource::Repo` (git+
  members build on the host) and a `repo =` deployment whose ply.toml is a
  composition converges the whole set via `converge_stack`.
- **Phase 2 — `ply ui`: DONE.** The deploy tab shows a composition's
  services as sub-rows, each with its own reconcile status; the guided
  new-deployment flow writes a `repo=` order.
- **Phase 3 — docs/naming: DONE.** `[[service]]` is the spelling (`[[app]]`
  alias kept, both-in-one-file is an error); stacks/deployments/model docs
  rewritten around recipe vs order, git+ members, and `repo=` compositions;
  `stack.toml` retired from the teaching path.
- **Phase 4 — DEFERRED:** per-service overrides in the order (a
  `[service.<name>]` table). Not needed to migrate production — the
  composition recipe carries per-service publish/env today. Revisit when a
  concrete need appears.

## Non-goals / YAGNI

- No inline topology in the order. Ever.
- No new "stack.ply" runner file — the order *is* that file.
- Per-service overrides are deferred until a concrete need appears.
