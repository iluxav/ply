# Registry: the manifest is the record — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A published version is its manifest plus a list of artifacts; the registry stores that record verbatim, derives everything else from it, and serves a file layout any static host can replicate.

**Architecture:** The CLI builds a *record* (`manifest_toml` + its JSON rendering + artifacts) and publishes it in one call; bytes go up in a separate mechanical upload. The registry (Next.js in `app/`, repo `iluxav/ply-web`, nested in this checkout and git-ignored) keeps one `records` table and regenerates `index.json`, `<name>-<version>.toml`, and `state.json` from it. Consumers (CLI, site, dashboard) read only those files. The old `X-Ply-Meta` header, the derived jsonb columns, and the hand-authored `registry/meta/*.json` retire after a one-off backfill.

**Tech Stack:** Rust 2021 workspace (`ply-core`, `ply-cli`; `toml`, `serde_json`, `ureq`), Next.js 15 app router + `postgres` (postgres.js) + `@aws-sdk/client-s3` (R2). New dev deps in `app/`: `vitest`, and one runtime dep `smol-toml` (TOML → JSON on the server, used only to validate a record and derive listing fields).

**Spec:** `docs/superpowers/specs/2026-09-02-registry-manifest-truth-design.md`

## Global Constraints

- **Never commit.** The repository owner commits both repos. `ply` tasks end with `make check` green (fmt + clippy `-D warnings` + workspace tests); `app/` tasks end with `npm test` (vitest), `npx tsc --noEmit`, and `npm run lint` green. Never run `npm run build` in a task (it syncs docs and takes minutes); never deploy; never push to the registry.
- The record shape, verbatim from the spec: `{owner, name, version, type, manifest_toml, manifest, artifacts:[{arch, src, sha256, bytes, verified}], pushed_at, published_by}`. `manifest` is a key-for-key JSON rendering of `manifest_toml` (no derivation, no defaults). `type` ∈ `app | layer | stack`: `[stack]` → stack; `[package] entrypoint` → app; else layer.
- Immutable per version: first publish fixes `manifest_toml`; later publishes must carry the identical text and may only add artifacts for arches not present; an artifact is never replaced. Re-publishing an identical artifact is idempotent (200).
- `verified` is per artifact: `true` only when plybox.sh stored and hashed the bytes; `false` for a publisher-supplied `src`. The server never fetches an external `src` in v1.
- Derived fields (server-side, from `manifest`), exactly: `type`; `description`/`license`/`homepage` from `package.*`; `volumes` = `volumes.*.path`; `links` = `requests.links`; `publish` = `internal:<port>` when `ports` has exactly one entry, else omitted; `dependencies` = `dependencies` as written (name → range string); `params` = `params` as written.
- Read-side layout, verbatim: `{owner}/{name}/index.json`, `{owner}/{name}/{name}-{version}.toml`, `{owner}/{name}/{name}-{version}-linux-{arch}.img`, `{owner}/state.json`, plus the root `state.json` merge. `state.json` v3 is additive over v2: per-artifact version entries keep `version, arch, img, src, sha256, bytes, pushed_at` and gain `manifest` (URL of the `.toml`), `verified`, and the derived fields; `apps` is no longer emitted.
- Endpoints: `POST /api/upload/` (bytes → `{src, sha256, bytes, verified:true}`), `POST /api/publish/` (record → 201/200/409/403/400). The old `POST /api/push/` keeps working for one CLI release and stores a record with `manifest_toml: ""`.
- Owner resolution: `[package] owner` / `[stack] owner` when present, else the token's login; the token must pass `canPublish` for it either way. `--as` sets the owner only when the manifest names none and conflicts otherwise.
- Secrets never appear in a record: a `{ secret = true }` param carries no value by construction; no task may add one.
- Tests: Rust in-file `#[cfg(test)]` with TOML string literals and `tempfile::tempdir()`, never the network. TypeScript with vitest over pure functions in `lib/`, no database, no network.

---

## File structure

| File | Responsibility |
|---|---|
| `app/lib/manifest.ts` (new) | TOML → JSON (`smol-toml`), `kindOf`, `derive` (the derived-fields table), record types |
| `app/lib/records.ts` (new) | `mergePublish` (pure publish rules) + `records` table access |
| `app/lib/catalog-files.ts` (new) | build `index.json`, `.toml`, per-owner and root `state.json` from records; pure builders + R2 writers |
| `app/lib/db.ts` (modify) | `records` and `uploads` tables in the migrate block |
| `app/app/api/upload/route.ts` (new) | bytes → R2, `uploads` row, returns `src` |
| `app/app/api/publish/route.ts` (new) | record → rules → `records` → regenerate files |
| `app/app/api/push/[[...target]]/route.ts` (modify) | legacy path: after its own insert, upsert a manifest-less record and regenerate via `catalog-files` |
| `app/lib/registry.ts` (modify) | `RegistryVersion` gains `manifest`, `verified`, `params`, `publish`; `apps` removed |
| `app/app/registry/[...slug]/page.tsx` (modify) | params/ports/volumes sections from derived fields; manifest link; verified mark |
| `app/package.json` (modify) | `smol-toml`, `vitest`, `"test": "vitest run"` |
| `ply-core/src/manifest.rs` (modify) | `[package] owner, description, license, homepage` |
| `ply-core/src/stack.rs` (modify) | `[stack] owner` |
| `ply-core/src/record.rs` (new) | `Record`, `Artifact`, `record_for_image`, `record_for_stack`, `expand_src`, `params_rows` |
| `ply-core/src/catalog.rs` (modify) | drop `PushMeta`/`derive_*`/`StackApp`; `ImageVersion` gains `manifest`, `verified`; `Package` gains `owner` |
| `ply-cli/src/commands/account.rs` (modify) | `push`: build → upload → publish; `--src`, `--arch`, `--dry-run` |
| `ply-cli/src/commands/images.rs` (modify) | `inspect REF|IMAGE|DIR` with params; `--json`, `--manifest` |
| `ply-cli/src/cli.rs` (modify) | `PushArgs`, `InspectArgs` |
| `scripts/registry-backfill-records.mjs` (new) | one-off: records for every published version via the CLI |
| `scripts/registry-push.mjs` (modify) | deb/apk lane uploads the `.toml` beside each image and emits v3 fields |
| `docs/registries.md`, `docs/manifest.md`, `docs/cli.md` (modify) | layout, protocol, self-hosting, new fields, new commands |

---

# Part A — Registry (`app/`, repo iluxav/ply-web)

All paths in Part A are relative to `app/`. Run commands from `app/`.

### Task 1: Manifest parsing and derived fields (`lib/manifest.ts`) + test runner

**Files:**
- Modify: `package.json` (deps + `test` script)
- Create: `lib/manifest.ts`
- Test: `lib/manifest.test.ts`

**Interfaces:**
- Produces:
  - `export type Artifact = { arch: "x64" | "arm64"; src: string; sha256: string; bytes: number; verified: boolean }`
  - `export type RecordKind = "app" | "layer" | "stack"`
  - `export type PublishRecord = { owner?: string; name: string; version: string; type?: RecordKind; manifest_toml: string; manifest: Manifest; artifacts: Artifact[]; backfill?: boolean }` — what `/api/publish/` receives
  - `export type StoredRecord = PublishRecord & { owner: string; type: RecordKind; pushed_at: string; published_by: string | null }`
  - `export type Manifest = Record<string, unknown>`
  - `export function manifestJson(toml: string): Manifest` — `smol-toml` parse; throws on invalid TOML
  - `export function kindOf(m: Manifest): RecordKind`
  - `export function identityOf(m: Manifest): { owner?: string; name: string; version: string }` — from `package.*` or `stack.*`; throws when name/version missing
  - `export type Derived = { description: string; license: string; homepage: string; volumes: string[]; links: string[]; publish?: string; dependencies: Record<string, string>; params: Record<string, unknown> }`
  - `export function derive(m: Manifest): Derived`
  - `export function sameJson(a: unknown, b: unknown): boolean` — deep-equal after key sort (used by publish to check the CLI's `manifest` against the server's parse)

- [ ] **Step 1: Add deps and the test script**

```bash
npm install smol-toml
npm install --save-dev vitest
```

In `package.json` scripts add `"test": "vitest run"`. Create `vitest.config.ts`:

```ts
import { defineConfig } from "vitest/config";
export default defineConfig({ test: { include: ["lib/**/*.test.ts"] } });
```

- [ ] **Step 2: Write the failing tests** — `lib/manifest.test.ts`

```ts
import { describe, expect, it } from "vitest";
import { derive, identityOf, kindOf, manifestJson, sameJson } from "./manifest";

const POSTGRES = `
[package]
name = "postgres"
owner = "ply"
version = "17.10.7"
description = "PostgreSQL relational database"
license = "PostgreSQL"
homepage = "https://www.postgresql.org"
entrypoint = ["./run.sh"]
base = "debian@13"

[dependencies]
postgresql17 = "17"
rclone = "1.60"

[volumes]
data = { path = "/var/lib/postgresql/data" }

[ports]
db = 5432

[params]
user = "postgres"
password = { secret = true }
url = "postgres://{user}:{password}@{host}:{port}/{database}"

[env]
POSTGRES_USER = "{user}"

[requests]
links = ["/var/run/docker.sock:/var/run/docker.sock"]
`;

const STACK = `
[stack]
name = "todos"
version = "0.1.0"

[[app]]
run = "postgres@17"
name = "db"
params = { database = "todos" }

[[app]]
run = "iluxav/todos-server@0.1"
e = ["DATABASE_URL={db.url}"]
`;

describe("manifestJson", () => {
  it("renders the toml key for key", () => {
    const m = manifestJson(POSTGRES);
    expect((m.package as { name: string }).name).toBe("postgres");
    expect((m.params as { password: { secret: boolean } }).password.secret).toBe(true);
    expect((m.env as { POSTGRES_USER: string }).POSTGRES_USER).toBe("{user}");
  });
  it("throws on invalid toml", () => {
    expect(() => manifestJson("[package\nname = 1")).toThrow();
  });
});

describe("kindOf and identityOf", () => {
  it("classifies app, layer and stack", () => {
    expect(kindOf(manifestJson(POSTGRES))).toBe("app");
    expect(kindOf(manifestJson('[package]\nname = "x"\nversion = "1.0.0"'))).toBe("layer");
    expect(kindOf(manifestJson(STACK))).toBe("stack");
  });
  it("reads identity from [package] or [stack]", () => {
    expect(identityOf(manifestJson(POSTGRES))).toEqual({ owner: "ply", name: "postgres", version: "17.10.7" });
    expect(identityOf(manifestJson(STACK))).toEqual({ owner: undefined, name: "todos", version: "0.1.0" });
    expect(() => identityOf(manifestJson('[package]\nname = "x"'))).toThrow(/version/);
  });
});

describe("derive", () => {
  it("computes every listing field from the manifest", () => {
    const d = derive(manifestJson(POSTGRES));
    expect(d.description).toBe("PostgreSQL relational database");
    expect(d.license).toBe("PostgreSQL");
    expect(d.homepage).toBe("https://www.postgresql.org");
    expect(d.volumes).toEqual(["/var/lib/postgresql/data"]);
    expect(d.links).toEqual(["/var/run/docker.sock:/var/run/docker.sock"]);
    expect(d.publish).toBe("internal:5432");
    expect(d.dependencies).toEqual({ postgresql17: "17", rclone: "1.60" });
    expect(d.params).toEqual({
      user: "postgres",
      password: { secret: true },
      url: "postgres://{user}:{password}@{host}:{port}/{database}",
    });
  });
  it("omits publish when ports has zero or several entries", () => {
    expect(derive(manifestJson('[package]\nname="a"\nversion="1.0.0"')).publish).toBeUndefined();
    expect(derive(manifestJson('[package]\nname="a"\nversion="1.0.0"\n[ports]\na=1\nb=2')).publish).toBeUndefined();
  });
  it("is empty-safe for a stack", () => {
    const d = derive(manifestJson(STACK));
    expect(d.volumes).toEqual([]);
    expect(d.params).toEqual({});
  });
});

describe("sameJson", () => {
  it("ignores key order and nothing else", () => {
    expect(sameJson({ a: 1, b: { c: [1, 2] } }, { b: { c: [1, 2] }, a: 1 })).toBe(true);
    expect(sameJson({ a: 1 }, { a: "1" })).toBe(false);
  });
});
```

- [ ] **Step 3: Run** — `npm test` → FAIL (module `./manifest` not found).

- [ ] **Step 4: Implement** — `lib/manifest.ts`

```ts
// The manifest is the record. This module is the ONLY place the server
// parses TOML: to check a CLI-supplied JSON rendering against the text, and
// to derive the listing fields state.json carries for consumers without a
// parser (the dashboard, the site). Nothing here invents data.
import { parse as parseToml } from "smol-toml";

export type Manifest = Record<string, unknown>;
export type RecordKind = "app" | "layer" | "stack";
export type Artifact = { arch: "x64" | "arm64"; src: string; sha256: string; bytes: number; verified: boolean };
export type PublishRecord = {
  owner?: string;
  name: string;
  version: string;
  type?: RecordKind;
  manifest_toml: string;
  manifest: Manifest;
  artifacts: Artifact[];
  backfill?: boolean;
};
export type StoredRecord = PublishRecord & {
  owner: string;
  type: RecordKind;
  pushed_at: string;
  published_by: string | null;
};

export function manifestJson(toml: string): Manifest {
  return parseToml(toml) as Manifest;
}

const table = (m: Manifest, key: string): Record<string, unknown> => {
  const v = m[key];
  return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : {};
};
const str = (v: unknown): string => (typeof v === "string" ? v : "");

export function kindOf(m: Manifest): RecordKind {
  if (m.stack) return "stack";
  return table(m, "package").entrypoint ? "app" : "layer";
}

export function identityOf(m: Manifest): { owner?: string; name: string; version: string } {
  const head = m.stack ? table(m, "stack") : table(m, "package");
  const name = str(head.name);
  const version = str(head.version);
  if (!name) throw new Error("manifest has no name");
  if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error(`manifest version must be x.y.z (got ${JSON.stringify(version)})`);
  const owner = str(head.owner) || undefined;
  return { owner, name, version };
}

export type Derived = {
  description: string;
  license: string;
  homepage: string;
  volumes: string[];
  links: string[];
  publish?: string;
  dependencies: Record<string, string>;
  params: Record<string, unknown>;
};

export function derive(m: Manifest): Derived {
  const pkg = table(m, "package");
  const volumes = Object.values(table(m, "volumes"))
    .map((v) => str((v as Record<string, unknown>)?.path))
    .filter(Boolean)
    .sort();
  const linksRaw = table(m, "requests").links;
  const links = Array.isArray(linksRaw)
    ? linksRaw.map((l) => (typeof l === "string" ? l : `${str((l as Record<string, unknown>).host)}:${str((l as Record<string, unknown>).at)}`))
    : [];
  const ports = Object.values(table(m, "ports"));
  const publish = ports.length === 1 && typeof ports[0] === "number" ? `internal:${ports[0]}` : undefined;
  const dependencies: Record<string, string> = {};
  for (const [k, v] of Object.entries(table(m, "dependencies"))) {
    dependencies[k] = typeof v === "string" ? v : str((v as Record<string, unknown>)?.version);
  }
  return {
    description: str(pkg.description),
    license: str(pkg.license),
    homepage: str(pkg.homepage),
    volumes,
    links,
    ...(publish ? { publish } : {}),
    dependencies,
    params: table(m, "params"),
  };
}

const canon = (v: unknown): unknown =>
  Array.isArray(v) ? v.map(canon)
  : v && typeof v === "object" ? Object.fromEntries(Object.keys(v as object).sort().map((k) => [k, canon((v as Record<string, unknown>)[k])]))
  : v;

export function sameJson(a: unknown, b: unknown): boolean {
  return JSON.stringify(canon(a)) === JSON.stringify(canon(b));
}
```

- [ ] **Step 5: Run** — `npm test` → PASS. Then `npx tsc --noEmit` and `npm run lint` clean.

---

### Task 2: Publish rules and the `records` table (`lib/records.ts`, `lib/db.ts`)

**Files:**
- Modify: `lib/db.ts` (migrate block, after the `events` table)
- Create: `lib/records.ts`
- Test: `lib/records.test.ts`

**Interfaces:**
- Consumes: Task 1 types.
- Produces:
  - `export type Existing = { manifest_toml: string; artifacts: Artifact[] } | null`
  - `export type Merge = { status: 201 | 200; artifacts: Artifact[] } | { status: 409 | 400; error: string; diff?: string }`
  - `export function mergePublish(existing: Existing, incoming: { manifest_toml: string; artifacts: Artifact[] }): Merge` — pure
  - `export function firstDiff(a: string, b: string): string` — `"line N: <a-line> | <b-line>"`
  - `export async function loadRecord(sql, owner, name, version): Promise<(StoredRecord & { id: number; package_id: number }) | null>`
  - `export async function saveRecord(sql, rec: StoredRecord & { package_id: number }, artifacts: Artifact[]): Promise<void>` — insert or update artifacts
  - `export async function listRecords(sql, owner): Promise<StoredRecord[]>`

- [ ] **Step 1: Failing tests** — `lib/records.test.ts`

```ts
import { describe, expect, it } from "vitest";
import { firstDiff, mergePublish } from "./records";
import type { Artifact } from "./manifest";

const x64: Artifact = { arch: "x64", src: "https://registry.plybox.sh/ply/a/a-1.0.0-linux-x64.img", sha256: "aa", bytes: 1, verified: true };
const arm: Artifact = { arch: "arm64", src: "https://registry.plybox.sh/ply/a/a-1.0.0-linux-arm64.img", sha256: "bb", bytes: 1, verified: true };

describe("mergePublish", () => {
  it("creates a version on first publish", () => {
    expect(mergePublish(null, { manifest_toml: "m", artifacts: [x64] })).toEqual({ status: 201, artifacts: [x64] });
  });
  it("appends a new arch to an existing version with the same manifest", () => {
    const r = mergePublish({ manifest_toml: "m", artifacts: [x64] }, { manifest_toml: "m", artifacts: [arm] });
    expect(r).toEqual({ status: 201, artifacts: [x64, arm] });
  });
  it("is idempotent for an identical artifact", () => {
    const r = mergePublish({ manifest_toml: "m", artifacts: [x64] }, { manifest_toml: "m", artifacts: [x64] });
    expect(r).toEqual({ status: 200, artifacts: [x64] });
  });
  it("refuses to replace an artifact with different bytes", () => {
    const r = mergePublish({ manifest_toml: "m", artifacts: [x64] }, { manifest_toml: "m", artifacts: [{ ...x64, sha256: "cc" }] });
    expect(r.status).toBe(409);
    expect((r as { error: string }).error).toMatch(/x64.*already published/);
  });
  it("refuses a different manifest for the same version, naming the first differing line", () => {
    const r = mergePublish({ manifest_toml: "a = 1\nb = 2\n", artifacts: [x64] }, { manifest_toml: "a = 1\nb = 3\n", artifacts: [arm] });
    expect(r.status).toBe(409);
    expect((r as { diff?: string }).diff).toBe("line 2: b = 2 | b = 3");
  });
  it("rejects a record with no artifacts unless it is a stack (empty allowed only on first publish)", () => {
    expect(mergePublish(null, { manifest_toml: "m", artifacts: [] })).toEqual({ status: 201, artifacts: [] });
    expect(mergePublish({ manifest_toml: "m", artifacts: [] }, { manifest_toml: "m", artifacts: [] })).toEqual({ status: 200, artifacts: [] });
  });
  it("rejects two artifacts for one arch in a single publish", () => {
    const r = mergePublish(null, { manifest_toml: "m", artifacts: [x64, { ...x64, sha256: "cc" }] });
    expect(r.status).toBe(400);
  });
});

describe("firstDiff", () => {
  it("names the first differing line", () => {
    expect(firstDiff("a\nb", "a\nc")).toBe("line 2: b | c");
    expect(firstDiff("a", "a\nb")).toBe("line 2: <end> | b");
  });
});
```

- [ ] **Step 2: Run** — `npm test` → FAIL.

- [ ] **Step 3: Implement** — `lib/records.ts`

```ts
// One row per (owner, name, version): the manifest verbatim, its JSON, and
// the artifacts. Publish rules are pure functions so they can be tested
// without a database; the SQL wrappers below are thin.
import type { Artifact, RecordKind, StoredRecord } from "./manifest";

export type Existing = { manifest_toml: string; artifacts: Artifact[] } | null;
export type Merge =
  | { status: 201 | 200; artifacts: Artifact[] }
  | { status: 409 | 400; error: string; diff?: string };

export function firstDiff(a: string, b: string): string {
  const la = a.split("\n"), lb = b.split("\n");
  const n = Math.max(la.length, lb.length);
  for (let i = 0; i < n; i++) {
    if (la[i] !== lb[i]) return `line ${i + 1}: ${la[i] ?? "<end>"} | ${lb[i] ?? "<end>"}`;
  }
  return "";
}

export function mergePublish(existing: Existing, incoming: { manifest_toml: string; artifacts: Artifact[] }): Merge {
  const seen = new Set<string>();
  for (const a of incoming.artifacts) {
    if (seen.has(a.arch)) return { status: 400, error: `two artifacts for ${a.arch} in one publish` };
    seen.add(a.arch);
  }
  if (!existing) return { status: 201, artifacts: incoming.artifacts };
  if (existing.manifest_toml !== incoming.manifest_toml) {
    return {
      status: 409,
      error: "this version is already published with a different manifest — one version, one manifest; bump the version",
      diff: firstDiff(existing.manifest_toml, incoming.manifest_toml),
    };
  }
  const merged = [...existing.artifacts];
  let added = 0;
  for (const a of incoming.artifacts) {
    const have = merged.find((e) => e.arch === a.arch);
    if (!have) { merged.push(a); added++; continue; }
    if (have.sha256 !== a.sha256) {
      return { status: 409, error: `${a.arch} is already published with different bytes — the registry is append-only; bump the version` };
    }
  }
  return { status: added ? 201 : 200, artifacts: merged };
}

// --- storage ---------------------------------------------------------------
// `sql` is the postgres.js client from lib/db.ts `ready()`.
type Sql = NonNullable<Awaited<ReturnType<typeof import("./db").ready>>>;

type Row = {
  id: number; package_id: number; owner: string; name: string; version: string; type: RecordKind;
  manifest_toml: string; manifest: Record<string, unknown>; artifacts: Artifact[];
  pushed_at: Date; published_by: string | null;
};

const fromRow = (r: Row): StoredRecord & { id: number; package_id: number } => ({
  id: r.id, package_id: r.package_id, owner: r.owner, name: r.name, version: r.version, type: r.type,
  manifest_toml: r.manifest_toml, manifest: r.manifest, artifacts: r.artifacts,
  pushed_at: r.pushed_at.toISOString(), published_by: r.published_by,
});

export async function loadRecord(sql: Sql, owner: string, name: string, version: string) {
  const rows = await sql<Row[]>`
    SELECT r.id, r.package_id, p.owner, p.name, r.version, r.type, r.manifest_toml, r.manifest, r.artifacts,
           r.pushed_at, u.username AS published_by
    FROM records r JOIN packages p ON p.id = r.package_id LEFT JOIN users u ON u.id = r.published_by
    WHERE p.owner = ${owner} AND p.name = ${name} AND r.version = ${version}`;
  return rows[0] ? fromRow(rows[0]) : null;
}

export async function saveRecord(
  sql: Sql,
  rec: { package_id: number; version: string; type: RecordKind; manifest_toml: string; manifest: Record<string, unknown>; published_by: number | null },
  artifacts: Artifact[],
) {
  await sql`
    INSERT INTO records (package_id, version, type, manifest_toml, manifest, artifacts, published_by)
    VALUES (${rec.package_id}, ${rec.version}, ${rec.type}, ${rec.manifest_toml}, ${sql.json(rec.manifest)}, ${sql.json(artifacts)}, ${rec.published_by})
    ON CONFLICT (package_id, version) DO UPDATE SET artifacts = EXCLUDED.artifacts`;
}

export async function listRecords(sql: Sql, owner: string): Promise<StoredRecord[]> {
  const rows = await sql<Row[]>`
    SELECT r.id, r.package_id, p.owner, p.name, r.version, r.type, r.manifest_toml, r.manifest, r.artifacts,
           r.pushed_at, u.username AS published_by
    FROM records r JOIN packages p ON p.id = r.package_id LEFT JOIN users u ON u.id = r.published_by
    WHERE p.owner = ${owner}
    ORDER BY p.name, r.pushed_at`;
  return rows.map(fromRow);
}
```

Add to `lib/db.ts` inside `ready()` after the `events` table:

```ts
    // v3: the manifest IS the record. One row per version; artifacts are a
    // jsonb list; every listing field is derived from `manifest` at write time.
    await s`
      CREATE TABLE IF NOT EXISTS records (
        id            serial PRIMARY KEY,
        package_id    int NOT NULL REFERENCES packages(id),
        version       text NOT NULL,
        type          text NOT NULL,
        manifest_toml text NOT NULL,
        manifest      jsonb NOT NULL,
        artifacts     jsonb NOT NULL DEFAULT '[]'::jsonb,
        pushed_at     timestamptz NOT NULL DEFAULT now(),
        published_by  int REFERENCES users(id),
        UNIQUE (package_id, version)
      )`;
    await s`CREATE INDEX IF NOT EXISTS records_manifest_gin ON records USING gin (manifest)`;
    // Bytes the registry stored itself: the only srcs a publish may mark verified.
    await s`
      CREATE TABLE IF NOT EXISTS uploads (
        key        text PRIMARY KEY,
        sha256     text NOT NULL,
        bytes      bigint NOT NULL,
        user_id    int NOT NULL REFERENCES users(id),
        created_at timestamptz NOT NULL DEFAULT now()
      )`;
```

- [ ] **Step 4: Run** — `npm test` → PASS; `npx tsc --noEmit`, `npm run lint` clean.

---

### Task 3: Catalog files from records (`lib/catalog-files.ts`)

**Files:**
- Create: `lib/catalog-files.ts`
- Test: `lib/catalog-files.test.ts`

**Interfaces:**
- Consumes: Task 1 (`derive`, `StoredRecord`), `putObject`/`getObject` from `lib/r2.ts`.
- Produces:
  - `export const tomlUrl = (owner, name, version) => \`https://registry.plybox.sh/${owner}/${name}/${name}-${version}.toml\``
  - `export function versionEntries(rec: StoredRecord): VersionEntry[]` — pure; one entry per artifact, or one entry for a stack
  - `export function ownerPackages(records: StoredRecord[]): OwnerPackage[]` — pure; groups by name, sorted by name then pushed_at
  - `export function indexFilenames(records: StoredRecord[], name: string): string[]` — stored image filenames only (plybox.sh-hosted artifacts) plus the `.toml` files
  - `export async function writeCatalogFiles(sql, owner: string, name: string): Promise<void>` — writes `{owner}/{name}/index.json`, every `{name}-{version}.toml` of that package, `{owner}/state.json`, then merges the root `state.json` under the advisory lock (the block moved out of the push route)

`VersionEntry` (v3, additive):

```ts
export type VersionEntry = {
  version: string; img: string | null; arch?: "x64" | "arm64"; src: string; sha256?: string; bytes: number;
  pushed_at: string; manifest: string; verified?: boolean;
  volumes?: string[]; links?: string[]; publish?: string; dependencies?: Record<string, string>; params?: Record<string, unknown>;
};
export type OwnerPackage = { namespace: string; owner: string; name: string; type: string; description: string; license: string; homepage: string; versions: VersionEntry[] };
```

- [ ] **Step 1: Failing tests** — `lib/catalog-files.test.ts`

```ts
import { describe, expect, it } from "vitest";
import { indexFilenames, ownerPackages, tomlUrl, versionEntries } from "./catalog-files";
import { manifestJson, type StoredRecord } from "./manifest";

const PG = `[package]\nname = "postgres"\nversion = "17.10.7"\ndescription = "db"\nentrypoint = ["./run.sh"]\n[ports]\ndb = 5432\n[params]\nuser = "postgres"\n`;
const rec = (over: Partial<StoredRecord> = {}): StoredRecord => ({
  owner: "ply", name: "postgres", version: "17.10.7", type: "app", manifest_toml: PG, manifest: manifestJson(PG),
  artifacts: [
    { arch: "x64", src: "https://registry.plybox.sh/ply/postgres/postgres-17.10.7-linux-x64.img", sha256: "aa", bytes: 10, verified: true },
    { arch: "arm64", src: "https://cdn.example.com/pg-arm64.img", sha256: "bb", bytes: 11, verified: false },
  ],
  pushed_at: "2026-09-02T15:51:37.000Z", published_by: "iluxav", ...over,
});

describe("versionEntries", () => {
  it("emits one v3 entry per artifact with the derived fields", () => {
    const [x64, arm] = versionEntries(rec());
    expect(x64).toMatchObject({
      version: "17.10.7", arch: "x64", img: "postgres-17.10.7-linux-x64.img",
      src: "https://registry.plybox.sh/ply/postgres/postgres-17.10.7-linux-x64.img", sha256: "aa", bytes: 10,
      pushed_at: "2026-09-02T15:51:37.000Z", manifest: tomlUrl("ply", "postgres", "17.10.7"), verified: true,
      publish: "internal:5432", params: { user: "postgres" },
    });
    expect(arm).toMatchObject({ arch: "arm64", img: "pg-arm64.img", verified: false });
    expect(x64).not.toHaveProperty("apps");
  });
  it("emits one image-less entry for a stack whose src is its toml", () => {
    const st = `[stack]\nname = "todos"\nversion = "0.1.0"\n[[app]]\nrun = "postgres@17"\n`;
    const [e] = versionEntries(rec({ name: "todos", version: "0.1.0", type: "stack", manifest_toml: st, manifest: manifestJson(st), artifacts: [] }));
    expect(e).toMatchObject({ img: null, src: tomlUrl("ply", "todos", "0.1.0"), manifest: tomlUrl("ply", "todos", "0.1.0"), bytes: 0 });
    expect(e.arch).toBeUndefined();
  });
});

describe("ownerPackages", () => {
  it("groups records by name and reads package fields from the manifest", () => {
    const pkgs = ownerPackages([rec(), rec({ version: "17.10.6", pushed_at: "2026-08-30T00:00:00.000Z" })]);
    expect(pkgs).toHaveLength(1);
    expect(pkgs[0]).toMatchObject({ namespace: "ply", owner: "ply", name: "postgres", type: "app", description: "db" });
    expect(pkgs[0].versions.map((v) => v.version)).toEqual(["17.10.6", "17.10.6", "17.10.7", "17.10.7"]);
  });
});

describe("indexFilenames", () => {
  it("lists stored images and the toml files, never external artifacts", () => {
    expect(indexFilenames([rec()], "postgres")).toEqual(["postgres-17.10.7-linux-x64.img", "postgres-17.10.7.toml"]);
  });
});
```

- [ ] **Step 2: Run** — FAIL.

- [ ] **Step 3: Implement** — `lib/catalog-files.ts`

```ts
// Everything a consumer reads is a file: index.json, the manifest, and the
// state snapshots. All of them are functions of the records table, so a
// rebuild is always possible and the database is never a consumer's concern.
import { derive, type StoredRecord } from "./manifest";
import { getObject, putObject } from "./r2";
import { listRecords } from "./records";

export const REGISTRY = "https://registry.plybox.sh";
export const tomlUrl = (owner: string, name: string, version: string) => `${REGISTRY}/${owner}/${name}/${name}-${version}.toml`;
const hosted = (src: string, owner: string, name: string) => src.startsWith(`${REGISTRY}/${owner}/${name}/`);
const basename = (u: string) => decodeURIComponent(u.split("/").at(-1) ?? "");

export type VersionEntry = {
  version: string; img: string | null; arch?: "x64" | "arm64"; src: string; sha256?: string; bytes: number;
  pushed_at: string; manifest: string; verified?: boolean;
  volumes?: string[]; links?: string[]; publish?: string; dependencies?: Record<string, string>; params?: Record<string, unknown>;
};
export type OwnerPackage = {
  namespace: string; owner: string; name: string; type: string; description: string; license: string; homepage: string; versions: VersionEntry[];
};

export function versionEntries(rec: StoredRecord): VersionEntry[] {
  const d = derive(rec.manifest);
  const manifest = tomlUrl(rec.owner, rec.name, rec.version);
  const extras = {
    ...(d.volumes.length ? { volumes: d.volumes } : {}),
    ...(d.links.length ? { links: d.links } : {}),
    ...(d.publish ? { publish: d.publish } : {}),
    ...(Object.keys(d.dependencies).length ? { dependencies: d.dependencies } : {}),
    ...(Object.keys(d.params).length ? { params: d.params } : {}),
  };
  if (rec.type === "stack" || rec.artifacts.length === 0) {
    return [{ version: rec.version, img: null, src: manifest, bytes: 0, pushed_at: rec.pushed_at, manifest, ...extras }];
  }
  return rec.artifacts.map((a) => ({
    version: rec.version, img: basename(a.src), arch: a.arch, src: a.src, sha256: a.sha256, bytes: a.bytes,
    pushed_at: rec.pushed_at, manifest, verified: a.verified, ...extras,
  }));
}

export function ownerPackages(records: StoredRecord[]): OwnerPackage[] {
  const byName = new Map<string, OwnerPackage>();
  const sorted = [...records].sort((a, b) => a.name.localeCompare(b.name) || a.pushed_at.localeCompare(b.pushed_at));
  for (const r of sorted) {
    const d = derive(r.manifest);
    const pkg = byName.get(r.name) ?? {
      namespace: r.owner, owner: r.owner, name: r.name, type: r.type,
      description: d.description, license: d.license, homepage: d.homepage, versions: [],
    };
    // the newest record's package fields win (records are sorted by pushed_at)
    Object.assign(pkg, { type: r.type, description: d.description, license: d.license, homepage: d.homepage });
    pkg.versions.push(...versionEntries(r));
    byName.set(r.name, pkg);
  }
  return [...byName.values()];
}

export function indexFilenames(records: StoredRecord[], name: string): string[] {
  const out: string[] = [];
  for (const r of records.filter((x) => x.name === name)) {
    for (const a of r.artifacts) if (hosted(a.src, r.owner, r.name)) out.push(basename(a.src));
    out.push(`${r.name}-${r.version}.toml`);
  }
  return out.sort();
}

type Sql = NonNullable<Awaited<ReturnType<typeof import("./db").ready>>>;

export async function writeCatalogFiles(sql: Sql, owner: string, name: string): Promise<void> {
  const records = await listRecords(sql, owner);
  for (const r of records.filter((x) => x.name === name)) {
    await putObject(`${owner}/${name}/${name}-${r.version}.toml`, r.manifest_toml, "application/toml", "public, max-age=31536000, immutable");
  }
  await putObject(`${owner}/${name}/index.json`, JSON.stringify(indexFilenames(records, name)), "application/json", "public, max-age=60");
  const ownerPkgs = ownerPackages(records);
  await putObject(`${owner}/state.json`, JSON.stringify({ updated: new Date().toISOString(), packages: ownerPkgs }, null, 1), "application/json", "public, max-age=60");
  await mergeRootState(sql, owner, ownerPkgs);
}

// The root catalog carries every namespace; a push replaces ONLY its own
// namespace's entries, serialized by an advisory lock. (Moved verbatim from
// the legacy push route — keep its semantics.)
export async function mergeRootState(sql: Sql, owner: string, ownerPkgs: OwnerPackage[]) {
  await sql.begin(async (tx) => {
    await tx`SELECT pg_advisory_xact_lock(771600)`;
    type Pkg = { namespace?: string; name?: string; versions?: { bytes?: number }[] };
    let root: { packages?: Pkg[] } = {};
    try { root = JSON.parse((await getObject("state.json")) ?? "{}"); } catch { /* first ever state */ }
    const kept = (root.packages ?? []).filter((p) => p.namespace !== owner);
    const packages = [...kept, ...(ownerPkgs as Pkg[])].sort((a, b) =>
      a.namespace === b.namespace ? (a.name ?? "").localeCompare(b.name ?? "") : (a.namespace ?? "").localeCompare(b.namespace ?? ""));
    const snapshot = {
      ...root,
      updated: new Date().toISOString().replace(/\.\d+Z$/, "Z"),
      package_count: packages.length,
      image_count: packages.reduce((n, p) => n + (p.versions?.length ?? 0), 0),
      total_bytes: packages.reduce((n, p) => n + (p.versions ?? []).reduce((m, v) => m + (v.bytes ?? 0), 0), 0),
      packages,
    };
    await putObject("state.json", JSON.stringify(snapshot, null, 1), "application/json", "public, max-age=30");
  });
}
```

- [ ] **Step 4: Run** — PASS; `tsc`, `lint` clean.

---

### Task 4: The endpoints — `/api/upload/`, `/api/publish/`, and the legacy `/api/push/` bridge

**Files:**
- Create: `app/api/upload/route.ts`
- Create: `app/api/publish/route.ts`
- Modify: `app/api/push/[[...target]]/route.ts` — replace its inline index/state regeneration with a manifest-less record + `writeCatalogFiles`
- Test: `lib/publish-checks.test.ts` for the pure request validation (`validatePublishBody`) extracted into `lib/records.ts`

**Interfaces:**
- Consumes: Tasks 1–3, `userForToken`, `canPublish`, `isReserved`.
- Produces (HTTP, verbatim from the spec):
  - `POST /api/upload/` — headers `Authorization`, `X-Ply-Filename` (must match `NAME_RE`), `X-Ply-Sha256`, optional `X-Ply-Namespace`; body bytes → `200 {src, sha256, bytes, verified: true}`; `400` when the computed sha256 differs from the header or the filename is not canonical; `409` when the key exists with different bytes; `200` idempotent when identical.
  - `POST /api/publish/` — JSON `PublishRecord` → `201|200 {record}`, `400 {error}`, `403 {error}`, `409 {error, diff?}`.
  - In `lib/records.ts`: `export function validatePublishBody(body: unknown): { ok: true; rec: PublishRecord } | { ok: false; error: string }` — shape check, `manifestJson(manifest_toml)` parses, `sameJson(parsed, rec.manifest)` (else `400 "manifest does not match manifest_toml"`), `identityOf(parsed)` equals `{name, version}` (else 400), artifacts each `{arch ∈ x64|arm64, src https, sha256 /^[0-9a-f]{64}$/, bytes ≥ 0}`.

- [ ] **Step 1: Failing tests** — `lib/publish-checks.test.ts`

```ts
import { describe, expect, it } from "vitest";
import { validatePublishBody } from "./records";
import { manifestJson } from "./manifest";

const toml = '[package]\nname = "a"\nversion = "1.0.0"\nentrypoint = ["x"]\n';
const good = { name: "a", version: "1.0.0", manifest_toml: toml, manifest: manifestJson(toml),
  artifacts: [{ arch: "x64", src: "https://h/a-1.0.0-linux-x64.img", sha256: "a".repeat(64), bytes: 1, verified: false }] };

describe("validatePublishBody", () => {
  it("accepts a well-formed record and derives type", () => {
    const r = validatePublishBody(good);
    expect(r.ok).toBe(true);
    if (r.ok) expect(r.rec.type).toBe("app");
  });
  it("rejects a manifest that does not match its toml", () => {
    const r = validatePublishBody({ ...good, manifest: { package: { name: "b", version: "1.0.0" } } });
    expect(r).toEqual({ ok: false, error: "manifest does not match manifest_toml" });
  });
  it("rejects a name/version that differs from the manifest", () => {
    expect(validatePublishBody({ ...good, version: "2.0.0" }).ok).toBe(false);
  });
  it("rejects malformed artifacts", () => {
    expect(validatePublishBody({ ...good, artifacts: [{ arch: "mips", src: "https://h/x", sha256: "zz", bytes: 1, verified: false }] }).ok).toBe(false);
    expect(validatePublishBody({ ...good, artifacts: [{ arch: "x64", src: "http://h/x.img", sha256: "a".repeat(64), bytes: 1, verified: false }] }).ok).toBe(false);
  });
  it("never trusts a client-supplied verified flag", () => {
    const r = validatePublishBody({ ...good, artifacts: [{ ...good.artifacts[0], verified: true }] });
    if (r.ok) expect(r.rec.artifacts[0].verified).toBe(false);
  });
});
```

- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.**

`lib/records.ts` addition:

```ts
import { identityOf, kindOf, manifestJson, sameJson, type PublishRecord, type Artifact } from "./manifest";

const SHA = /^[0-9a-f]{64}$/;
export function validatePublishBody(body: unknown): { ok: true; rec: PublishRecord } | { ok: false; error: string } {
  const b = body as Partial<PublishRecord> | null;
  if (!b || typeof b !== "object") return { ok: false, error: "expected a JSON record" };
  if (typeof b.manifest_toml !== "string" || !b.manifest_toml.trim()) return { ok: false, error: "manifest_toml is required" };
  let parsed;
  try { parsed = manifestJson(b.manifest_toml); } catch (e) { return { ok: false, error: `manifest_toml: ${(e as Error).message}` }; }
  if (!sameJson(parsed, b.manifest)) return { ok: false, error: "manifest does not match manifest_toml" };
  let id;
  try { id = identityOf(parsed); } catch (e) { return { ok: false, error: (e as Error).message }; }
  if (b.name !== id.name || b.version !== id.version) return { ok: false, error: `record names ${b.name}@${b.version} but the manifest says ${id.name}@${id.version}` };
  const artifacts: Artifact[] = [];
  for (const a of Array.isArray(b.artifacts) ? b.artifacts : []) {
    const x = a as Partial<Artifact>;
    if (x.arch !== "x64" && x.arch !== "arm64") return { ok: false, error: "artifact arch must be x64 or arm64" };
    if (typeof x.src !== "string" || !x.src.startsWith("https://")) return { ok: false, error: "artifact src must be an https URL" };
    if (typeof x.sha256 !== "string" || !SHA.test(x.sha256)) return { ok: false, error: "artifact sha256 must be 64 hex chars" };
    if (typeof x.bytes !== "number" || x.bytes < 0) return { ok: false, error: "artifact bytes must be a non-negative number" };
    artifacts.push({ arch: x.arch, src: x.src, sha256: x.sha256, bytes: x.bytes, verified: false }); // the server decides `verified`
  }
  return { ok: true, rec: { owner: id.owner ?? (typeof b.owner === "string" ? b.owner : undefined), name: id.name, version: id.version, type: kindOf(parsed), manifest_toml: b.manifest_toml, manifest: parsed, artifacts, backfill: b.backfill === true } };
}
```

`app/api/upload/route.ts`:

```ts
// Bytes are mechanical. This stores an image under {owner}/{name}/ and hands
// back the src a publish may cite as verified. Nothing is cataloged here.
import { NextResponse } from "next/server";
import { createHash } from "node:crypto";
import { userForToken } from "@/lib/auth";
import { canPublish } from "@/lib/namespaces";
import { ready } from "@/lib/db";
import { putObject } from "@/lib/r2";
import { REGISTRY } from "@/lib/catalog-files";

const MAX_BYTES = 512 * 1024 * 1024;
const NAME_RE = /^([a-z0-9][a-z0-9-]*)-(\d+\.\d+\.\d+)-linux-(x64|arm64)\.img$/;

export async function POST(req: Request) {
  const user = await userForToken((req.headers.get("authorization") ?? "").replace(/^Bearer\s+/i, ""));
  if (!user) return NextResponse.json({ error: "publish with a key: Authorization: Bearer ply_…" }, { status: 401 });
  const filename = req.headers.get("x-ply-filename") ?? "";
  const m = NAME_RE.exec(filename);
  if (!m) return NextResponse.json({ error: "X-Ply-Filename must be <name>-<x.y.z>-linux-<x64|arm64>.img" }, { status: 400 });
  const owner = (req.headers.get("x-ply-namespace") ?? user.username ?? "").toLowerCase();
  if (!owner) return NextResponse.json({ error: "choose your username first at plybox.sh/account" }, { status: 409 });
  if (!(await canPublish(user.id, user.username, owner))) return NextResponse.json({ error: `you cannot publish to \`${owner}\`` }, { status: 403 });
  const sql = await ready();
  if (!sql) return NextResponse.json({ error: "registry accounts are not enabled here" }, { status: 503 });
  if (!req.body) return NextResponse.json({ error: "empty body" }, { status: 400 });

  const chunks: Buffer[] = []; const hash = createHash("sha256"); let total = 0;
  const reader = req.body.getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > MAX_BYTES) return NextResponse.json({ error: "image exceeds 512MB" }, { status: 413 });
    hash.update(value); chunks.push(Buffer.from(value));
  }
  if (total === 0) return NextResponse.json({ error: "empty body" }, { status: 400 });
  const sha256 = hash.digest("hex");
  const claimed = req.headers.get("x-ply-sha256");
  if (claimed && claimed !== sha256) return NextResponse.json({ error: `sha256 mismatch: you said ${claimed.slice(0, 12)}…, the bytes are ${sha256.slice(0, 12)}…` }, { status: 400 });

  const key = `${owner}/${m[1]}/${filename}`;
  const [existing] = await sql`SELECT sha256 FROM uploads WHERE key = ${key}`;
  if (existing && existing.sha256 !== sha256) return NextResponse.json({ error: `${filename} already exists under ${owner}/ with different bytes — bump the version` }, { status: 409 });
  if (!existing) {
    await putObject(key, Buffer.concat(chunks), "application/octet-stream", "public, max-age=31536000, immutable");
    await sql`INSERT INTO uploads (key, sha256, bytes, user_id) VALUES (${key}, ${sha256}, ${total}, ${user.id})`;
  }
  return NextResponse.json({ src: `${REGISTRY}/${key}`, sha256, bytes: total, verified: true });
}
```

`app/api/publish/route.ts`:

```ts
// The publish: a record in, files out. Rules live in lib/records.ts.
import { NextResponse } from "next/server";
import { userForToken } from "@/lib/auth";
import { canPublish, isReserved } from "@/lib/namespaces";
import { ready } from "@/lib/db";
import { loadRecord, mergePublish, saveRecord, validatePublishBody } from "@/lib/records";
import { writeCatalogFiles, REGISTRY } from "@/lib/catalog-files";

export async function POST(req: Request) {
  const user = await userForToken((req.headers.get("authorization") ?? "").replace(/^Bearer\s+/i, ""));
  if (!user) return NextResponse.json({ error: "publish with a key: Authorization: Bearer ply_…" }, { status: 401 });
  const body = await req.json().catch(() => null);
  const v = validatePublishBody(body);
  if (!v.ok) return NextResponse.json({ error: v.error }, { status: 400 });
  const rec = v.rec;
  const owner = (rec.owner ?? user.username ?? "").toLowerCase();
  if (!owner) return NextResponse.json({ error: "choose your username first at plybox.sh/account — it becomes your namespace" }, { status: 409 });
  if (!(await canPublish(user.id, user.username, owner))) {
    return NextResponse.json({ error: isReserved(owner) ? `\`${owner}\` is an official namespace — publishing there needs a grant` : `you cannot publish to \`${owner}\`` }, { status: 403 });
  }
  const sql = await ready();
  if (!sql) return NextResponse.json({ error: "registry accounts are not enabled here" }, { status: 503 });

  // `verified` is the server's word: only bytes it stored itself.
  for (const a of rec.artifacts) {
    if (a.src.startsWith(`${REGISTRY}/${owner}/${rec.name}/`)) {
      const key = a.src.slice(`${REGISTRY}/`.length);
      const [up] = await sql`SELECT sha256 FROM uploads WHERE key = ${key}`;
      if (!up) {
        // backfill: a version the legacy push stored — trust its row once
        const [legacy] = rec.backfill ? await sql`
          SELECT v.sha256 FROM versions v JOIN packages p ON p.id = v.package_id
          WHERE p.owner = ${owner} AND p.name = ${rec.name} AND v.version = ${rec.version} AND v.arch = ${a.arch}` : [];
        if (!legacy) return NextResponse.json({ error: `${a.src} was never uploaded here — upload it first, or point src at where the bytes live` }, { status: 400 });
        a.verified = legacy.sha256 === a.sha256;
      } else {
        if (up.sha256 !== a.sha256) return NextResponse.json({ error: `${a.src}: stored bytes have sha256 ${up.sha256.slice(0, 12)}…, the record says ${a.sha256.slice(0, 12)}…` }, { status: 400 });
        a.verified = true;
      }
    }
  }

  const [pkg] = await sql`
    INSERT INTO packages (owner, name, type) VALUES (${owner}, ${rec.name}, ${rec.type})
    ON CONFLICT (owner, name) DO UPDATE SET type = ${rec.type}
    RETURNING id`;
  const existing = await loadRecord(sql, owner, rec.name, rec.version);
  const merge = mergePublish(existing ? { manifest_toml: existing.manifest_toml, artifacts: existing.artifacts } : null, rec);
  if (merge.status === 409 || merge.status === 400) return NextResponse.json({ error: merge.error, ...(merge.diff ? { diff: merge.diff } : {}) }, { status: merge.status });
  await saveRecord(sql, { package_id: pkg.id, version: rec.version, type: rec.type!, manifest_toml: rec.manifest_toml, manifest: rec.manifest, published_by: user.id }, merge.artifacts);
  if (merge.status === 201) await sql`INSERT INTO events (kind, owner, name, version) VALUES ('push', ${owner}, ${rec.name}, ${rec.version})`;
  await writeCatalogFiles(sql, owner, rec.name);
  const stored = await loadRecord(sql, owner, rec.name, rec.version);
  return NextResponse.json(stored, { status: merge.status });
}
```

Legacy bridge in `app/api/push/[[...target]]/route.ts`: after the `INSERT INTO versions …` and the `events` insert, replace everything from the comment `// regenerate this package's index + the owner's catalog` to the end of the root-state block with:

```ts
  // v3 bridge (one release): a manifest-less record so the catalog files —
  // now generated from records only — still list this version. The backfill
  // script fills manifest_toml in; the page says "re-push with ply ≥ 0.1.70".
  const existing = await loadRecord(sql, owner, name, version);
  const artifact = isStack ? [] : [{ arch: arch as "x64" | "arm64", src: origin?.url ?? `${REGISTRY}/${owner}/${name}/${filename}`, sha256, bytes: total, verified: !origin }];
  const merged = existing ? [...existing.artifacts.filter((a) => a.arch !== arch), ...artifact] : artifact;
  await saveRecord(sql, { package_id: pkg.id, version, type: meta.type as "app" | "layer" | "stack", manifest_toml: existing?.manifest_toml ?? "", manifest: existing?.manifest ?? {}, published_by: user.id }, merged);
  if (!origin && !isStack) await sql`INSERT INTO uploads (key, sha256, bytes, user_id) VALUES (${`${owner}/${name}/${filename}`}, ${sha256}, ${total}, ${user.id}) ON CONFLICT (key) DO NOTHING`;
  await writeCatalogFiles(sql, owner, name);
```

(`versionEntries` in Task 3 must tolerate `manifest_toml: ""` / `manifest: {}`: `derive({})` already returns empties; add to `versionEntries`: when `rec.manifest_toml === ""` omit the `manifest` URL — write a test for it in `catalog-files.test.ts`: `it("omits the manifest url for a legacy record without one")`.)

- [ ] **Step 4: Run** — `npm test` PASS; `npx tsc --noEmit`; `npm run lint`.

---

### Task 5: The package page renders the manifest

**Files:**
- Modify: `lib/registry.ts` (`RegistryVersion`: add `manifest?: string; verified?: boolean; publish?: string; params?: Record<string, unknown>; dependencies?: Record<string, string> | { name: string; version: string }[]`; drop `apps` and `StackApp`)
- Modify: `app/registry/[...slug]/page.tsx`
- Test: `lib/params-rows.test.ts` for the pure row builder

**Interfaces:**
- Produces in `lib/registry.ts`: `export type ParamRow = { name: string; kind: "default" | "computed" | "secret (minted)" | "secret (external)"; value?: string }` and `export function paramRows(params: Record<string, unknown> | undefined): ParamRow[]`; `export const depsOf = (v: RegistryVersion): { name: string; version: string }[]` normalizing both the v2 array and the v3 object.

- [ ] **Step 1: Failing test** — `lib/params-rows.test.ts`

```ts
import { describe, expect, it } from "vitest";
import { depsOf, paramRows } from "./registry";

describe("paramRows", () => {
  it("classifies every declaration kind and never carries a secret value", () => {
    expect(paramRows({ user: "postgres", url: "x://{host}", password: { secret: true }, key: { secret: true, external: true } })).toEqual([
      { name: "key", kind: "secret (external)" },
      { name: "password", kind: "secret (minted)" },
      { name: "url", kind: "computed", value: "x://{host}" },
      { name: "user", kind: "default", value: "postgres" },
    ]);
    expect(paramRows(undefined)).toEqual([]);
  });
});
describe("depsOf", () => {
  it("reads v2 arrays and v3 objects alike", () => {
    expect(depsOf({ version: "1", img: null, bytes: 0, pushed_at: "", dependencies: [{ name: "a", version: "1.0.0" }] })).toEqual([{ name: "a", version: "1.0.0" }]);
    expect(depsOf({ version: "1", img: null, bytes: 0, pushed_at: "", dependencies: { a: "1" } })).toEqual([{ name: "a", version: "1" }]);
  });
});
```

- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.**

In `lib/registry.ts`:

```ts
export type ParamRow = { name: string; kind: "default" | "computed" | "secret (minted)" | "secret (external)"; value?: string };
export function paramRows(params: Record<string, unknown> | undefined): ParamRow[] {
  return Object.entries(params ?? {}).sort(([a], [b]) => a.localeCompare(b)).map(([name, v]) => {
    if (typeof v === "string") return v.includes("{") ? { name, kind: "computed", value: v } : { name, kind: "default", value: v };
    const t = (v ?? {}) as { secret?: boolean; external?: boolean };
    return { name, kind: t.external ? "secret (external)" : "secret (minted)" };
  });
}
export const depsOf = (v: RegistryVersion): { name: string; version: string }[] =>
  Array.isArray(v.dependencies) ? v.dependencies : Object.entries(v.dependencies ?? {}).map(([name, version]) => ({ name, version }));
```

In the page, after the "use it" block and before "versions", using `latest` (the newest entry):

```tsx
      {paramRows(latest.params).length > 0 && (
        <>
          <h2 className="mt-10 font-mono text-[10px] uppercase tracking-wider text-fade">params</h2>
          <p className="mt-1 font-mono text-xs text-fade">reference as {"{"}{p.name}.&lt;name&gt;{"}"} from a stack; set with params = {"{ <name> = \"…\" }"}</p>
          <div className="mt-2 overflow-x-auto border border-edge">
            <table className="w-full text-sm">
              <tbody>
                {paramRows(latest.params).map((r) => (
                  <tr key={r.name} className="border-b border-edge last:border-b-0">
                    <td className="whitespace-nowrap px-4 py-2 font-mono">{r.name}</td>
                    <td className="px-4 py-2 font-mono text-xs text-fade">{r.kind}</td>
                    <td className="px-4 py-2 font-mono text-xs">{r.value ?? ""}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
      {(latest.publish || latest.volumes?.length || depsOf(latest).length > 0) && (
        <p className="mt-6 font-mono text-xs text-fade">
          {latest.publish && <span className="mr-4">publish: {latest.publish}</span>}
          {latest.volumes?.length ? <span className="mr-4">volumes: {latest.volumes.join(", ")}</span> : null}
          {depsOf(latest).length > 0 && <span>depends on: {depsOf(latest).map((d) => `${d.name} ${d.version}`).join(", ")}</span>}
        </p>
      )}
      {latest.manifest && (
        <p className="mt-2 font-mono text-xs text-fade">
          <a href={latest.manifest} className="text-accent hover:underline">ply.toml</a> — the manifest as published
        </p>
      )}
```

In the versions table, after the arch cell, add a cell: `{v.verified === false ? <span title="bytes hosted by the publisher; sha256 reported by their ply">external</span> : ""}`.

- [ ] **Step 4: Run** — PASS; `tsc`; `lint`.

---

# Part B — CLI (`ply` repo root)

### Task 6: `[package] owner/description/license/homepage` and `[stack] owner`

**Files:**
- Modify: `ply-core/src/manifest.rs` (`Package` struct)
- Modify: `ply-core/src/stack.rs` (`Stack` struct, the `[stack]` key allowlist at ~`:360`, and the parse of the header)
- Test: in-file.

**Interfaces:**
- Produces: `Package { pub owner: Option<String>, pub description: Option<String>, pub license: Option<String>, pub homepage: Option<String>, … }` (all `#[serde(default, skip_serializing_if = "Option::is_none")]`, placed before `base` which must stay last); `Stack { pub owner: Option<String>, … }`.

- [ ] **Step 1: Failing tests**

```rust
// manifest.rs tests
#[test]
fn package_carries_registry_facing_fields() {
    let m = Manifest::parse(r#"
[package]
name = "postgres"
owner = "ply"
version = "17.10.7"
description = "PostgreSQL relational database"
license = "PostgreSQL"
homepage = "https://www.postgresql.org"
"#).unwrap();
    assert_eq!(m.package.owner.as_deref(), Some("ply"));
    assert_eq!(m.package.license.as_deref(), Some("PostgreSQL"));
    let back = toml::to_string(&m).unwrap();
    assert!(back.contains("owner = \"ply\""), "round-trips: {back}");
}

// stack.rs tests
#[test]
fn stack_header_accepts_owner() {
    let s = parse("[stack]\nname = \"todos\"\nowner = \"iluxav\"\nversion = \"0.1.0\"\n[[app]]\nrun = \"postgres@17\"\n", Path::new("stack.toml")).unwrap().unwrap();
    assert_eq!(s.owner.as_deref(), Some("iluxav"));
}
```

- [ ] **Step 2: Run** — `cargo test -p ply-core package_carries` / `stack_header_accepts_owner` → FAIL (unknown field / no such field).
- [ ] **Step 3: Implement** — add the four `Option<String>` fields to `Package` (before `base`), read `owner` in `stack.rs`'s header parse and add `"owner"` to the allowed key list and its error message.
- [ ] **Step 4: Run** — PASS. **Step 5:** `make check`.

---

### Task 7: The record (`ply-core/src/record.rs`)

**Files:**
- Create: `ply-core/src/record.rs`; add `pub mod record;` to `ply-core/src/lib.rs`
- Test: in-file.

**Interfaces:**
- Consumes: `image::read::{read_embedded, MANIFEST_PATH}`, `Manifest::parse`, `stack::parse`, `catalog::PackageKind`, Task 6 fields.
- Produces:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact { pub arch: String, pub src: String, pub sha256: String, pub bytes: u64, pub verified: bool }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub owner: Option<String>,
    pub name: String,
    pub version: String,
    #[serde(rename = "type")] pub kind: PackageKind,
    pub manifest_toml: String,
    pub manifest: serde_json::Value,
    pub artifacts: Vec<Artifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub pushed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub published_by: Option<String>,
}

/// The record for a built image: manifest text read from `/.manifest.toml`
/// INSIDE the image (what the artifact really contains), JSON rendered from it.
pub fn record_for_image(image: &Path) -> Result<Record>;
/// The record for a manifest text that is not (yet) an image: a stack file,
/// or a dir's ply.toml for `inspect`.
pub fn record_for_toml(text: &str, path: &Path) -> Result<Record>;
/// `{version}` / `{arch}` in a `--src` template.
pub fn expand_src(template: &str, version: &str, arch: &str) -> String;
/// Display rows for `[params]`: (name, kind, value) where kind ∈ "default" | "computed" | "secret, minted" | "secret, external".
pub fn params_rows(manifest: &serde_json::Value) -> Vec<(String, &'static str, Option<String>)>;
/// Built-in facts and live names, for `inspect`'s footer.
pub const FACTS: &[&str] = &["name", "version", "host", "port", "addr", "base_url", "scale", "arch", "image"];
```

`kind` derivation: text contains a `[stack]` table → Stack; `package.entrypoint` present → App; else Layer. `owner` = `package.owner` / `stack.owner`. `manifest` = `serde_json::to_value(text.parse::<toml::Value>()?)` — key for key; a TOML datetime renders as its string. The image's manifest is also validated via `Manifest::parse` (a `deny_unknown_fields` failure is a hard error naming the field: an old binary meets a new field here).

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    const PG: &str = r#"[package]
name = "postgres"
owner = "ply"
version = "17.10.7"
entrypoint = ["./run.sh"]
[params]
user = "postgres"
password = { secret = true }
key = { secret = true, external = true }
url = "postgres://{user}:{password}@{host}:{port}/{database}"
"#;
    #[test]
    fn a_toml_becomes_a_record_key_for_key() {
        let r = record_for_toml(PG, Path::new("ply.toml")).unwrap();
        assert_eq!((r.owner.as_deref(), r.name.as_str(), r.version.as_str()), (Some("ply"), "postgres", "17.10.7"));
        assert_eq!(r.kind, crate::catalog::PackageKind::App);
        assert_eq!(r.manifest["params"]["password"]["secret"], serde_json::json!(true));
        assert_eq!(r.manifest["package"]["entrypoint"][0], serde_json::json!("./run.sh"));
        assert!(r.artifacts.is_empty());
        assert_eq!(r.manifest_toml, PG);
    }
    #[test]
    fn a_layer_and_a_stack_are_classified() {
        assert_eq!(record_for_toml("[package]\nname = \"x\"\nversion = \"1.0.0\"\n", Path::new("p")).unwrap().kind, crate::catalog::PackageKind::Layer);
        let s = record_for_toml("[stack]\nname = \"todos\"\nversion = \"0.1.0\"\n[[app]]\nrun = \"postgres@17\"\n", Path::new("stack.toml")).unwrap();
        assert_eq!(s.kind, crate::catalog::PackageKind::Stack);
        assert_eq!(s.name, "todos");
    }
    #[test]
    fn an_image_record_reads_the_embedded_manifest() {
        // build a tiny image the way manifest/read tests do (see image::squashfs tests for the helper that packs `/.manifest.toml`)
        let td = tempfile::tempdir().unwrap();
        let img = crate::image::squashfs::test_image_with_manifest(td.path(), PG); // add this test helper if absent (pub(crate) under cfg(test))
        let r = record_for_image(&img).unwrap();
        assert_eq!(r.name, "postgres");
        assert_eq!(r.manifest_toml, PG);
    }
    #[test]
    fn src_templates_expand_version_and_arch() {
        assert_eq!(expand_src("https://h/pg-{version}-linux-{arch}.img", "17.10.7", "x64"), "https://h/pg-17.10.7-linux-x64.img");
        assert_eq!(expand_src("https://h/fixed.img", "1", "arm64"), "https://h/fixed.img");
    }
    #[test]
    fn params_rows_never_carry_a_secret_value() {
        let r = record_for_toml(PG, Path::new("p")).unwrap();
        let rows = params_rows(&r.manifest);
        assert_eq!(rows.iter().find(|r| r.0 == "password").unwrap().1, "secret, minted");
        assert_eq!(rows.iter().find(|r| r.0 == "key").unwrap().1, "secret, external");
        assert_eq!(rows.iter().find(|r| r.0 == "url").unwrap().1, "computed");
        assert!(rows.iter().all(|r| r.1.starts_with("secret") == r.2.is_none()));
    }
}
```

- [ ] **Step 2: Run** — FAIL. **Step 3: Implement** `record.rs` per the interface (~120 lines; `record_for_image` = `read_embedded(image, MANIFEST_PATH)?` → UTF-8 → `record_for_toml`; `Manifest::parse(text)` for validation when the text has no `[stack]`, `stack::parse(text, path)` otherwise). **Step 4:** PASS. **Step 5:** `make check`.

---

### Task 8: `ply push` on the record protocol

**Files:**
- Modify: `ply-cli/src/cli.rs` (`PushArgs`)
- Modify: `ply-cli/src/commands/account.rs` (`push`, delete `push_url`, `push_stack`, `load_stack_for_push` stays)
- Modify: `ply-core/src/catalog.rs` (delete `PushMeta`, `derive_push_meta`, `derive_stack_meta`, `StackApp`, `Dep` and their tests)
- Test: `account.rs` in-file tests for the pure planning fn; the network path is thin.

**Interfaces:**
- `PushArgs { pub target: String /* image, dir, stack.toml */, pub as_namespace: Option<String>, pub src: Option<String>, pub arch: Option<String>, pub dry_run: bool }` — help text per the spec's table.
- Produces in `account.rs`: `pub struct PushPlan { pub record: Record, pub image: Option<PathBuf>, pub upload: bool }` and `fn plan_push(target: &Path, src: Option<&str>, arch: Option<&str>, as_namespace: Option<&str>) -> Result<PushPlan>` (pure apart from reading/building the image). Then `push()` = plan → `--dry-run` prints `serde_json::to_string_pretty(&plan.record)` → else upload (`POST /api/upload/` with `X-Ply-Filename`, `X-Ply-Sha256`, `X-Ply-Namespace` when `--as`) → set the artifact `src` from the response → `POST /api/publish/` JSON → print `published {owner}/{name}@{version}`, the `.toml` URL, and for an app the same `use:` snippet as today.
- Building a dir: mirror `commands/build.rs::run`'s `BuildOptions` construction (read it), output to `<dir>/<name>-<version>-linux-<arch>.img`, arch from `--arch` or the host.
- Owner resolution: `record.owner` from the manifest; `--as` fills it when `None`; both present and different → `bail!("manifest says owner = \"{a}\" but --as {b} was given")`.
- `--src`: `expand_src(template, version, arch)`; `sha256_file` + `metadata().len()` for the artifact; `upload = false`.

- [ ] **Step 1: Failing tests** (`account.rs` `#[cfg(test)]`)

```rust
#[test]
fn a_stack_file_plans_a_record_with_no_artifacts_and_no_upload() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("stack.toml");
    std::fs::write(&f, "[stack]\nname = \"todos\"\nversion = \"0.1.0\"\n[[app]]\nrun = \"postgres@17\"\n").unwrap();
    let p = plan_push(&f, None, None, Some("iluxav")).unwrap();
    assert_eq!(p.record.kind, ply_core::catalog::PackageKind::Stack);
    assert_eq!(p.record.owner.as_deref(), Some("iluxav"));
    assert!(p.record.artifacts.is_empty() && p.image.is_none() && !p.upload);
}
#[test]
fn a_stack_with_a_local_member_is_refused() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("stack.toml");
    std::fs::write(&f, "[stack]\nname = \"t\"\nversion = \"0.1.0\"\n[[app]]\nrun = \"./server\"\n").unwrap();
    let e = plan_push(&f, None, None, None).unwrap_err().to_string();
    assert!(e.contains("./server"), "{e}");
}
#[test]
fn an_image_with_src_plans_an_unverified_external_artifact_and_no_upload() {
    let td = tempfile::tempdir().unwrap();
    let img = ply_core::image::squashfs::test_image_with_manifest(td.path(), "[package]\nname = \"a\"\nversion = \"1.0.0\"\nentrypoint = [\"x\"]\n");
    let p = plan_push(&img, Some("https://h/a-{version}-linux-{arch}.img"), Some("x64"), None).unwrap();
    let a = &p.record.artifacts[0];
    assert_eq!((a.arch.as_str(), a.src.as_str(), a.verified), ("x64", "https://h/a-1.0.0-linux-x64.img", false));
    assert_eq!(a.sha256, ply_core::digest::sha256_file(&img).unwrap());
    assert!(!p.upload);
}
#[test]
fn as_conflicts_with_a_manifest_owner() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("stack.toml");
    std::fs::write(&f, "[stack]\nname = \"t\"\nowner = \"ply\"\nversion = \"0.1.0\"\n[[app]]\nrun = \"postgres@17\"\n").unwrap();
    assert!(plan_push(&f, None, None, Some("iluxav")).unwrap_err().to_string().contains("--as"));
}
```

- [ ] **Step 2: Run** — `cargo test -p ply-cli account::` → FAIL. **Step 3: Implement** (delete the `X-Ply-Meta` path entirely; the old `push_url` behavior is now `push URL --src URL`? No: a bare https target is no longer accepted — `ply push` needs the manifest, which only the image or the toml carries; print `"to publish an image hosted elsewhere: ply push ./the.img --src https://…"`). **Step 4:** PASS. **Step 5:** `make check` (the `catalog.rs` deletions will surface every dead reference; remove them).

---

### Task 9: Catalog v3 fields and `ply inspect`

**Files:**
- Modify: `ply-core/src/catalog.rs` (`ImageVersion`: `+ manifest: String` (default), `+ verified: Option<bool>`; keep `volumes/links`, change `dependencies` to `serde_json::Value` default-null tolerated for both shapes, drop `apps`; `Package`: `+ owner: String` default)
- Modify: `ply-cli/src/cli.rs` (`InspectArgs { target: String, json: bool, manifest: bool }`)
- Modify: `ply-cli/src/commands/images.rs` (`inspect`)
- Test: `catalog.rs` in-file (v2 and v3 state parse), `images.rs` in-file (render).

**Interfaces:**
- `pub fn inspect(args: InspectArgs) -> Result<()>`: target resolution — a path that exists: `.img` → `record_for_image`; dir → stack (`stack::load`) or `ply.toml` → `record_for_toml`; `.toml` file → `record_for_toml`; else a registry ref → `catalog::fetch_app_image(name, want, OFFICIAL_RUN_SOURCE)` → `record_for_image`. `--json` prints the record; `--manifest` prints `manifest_toml`; default prints:

```
postgres 17.10.7  app  owner: ply
volumes:      /var/lib/postgresql/data
links:        —
dependencies: postgresql17 17, rclone 1.60
params:       reference as {postgres.<name>} from a stack; set with params = { <name> = "…" }
  user      default   postgres
  database  default   postgres
  password  secret, minted
  url       computed  postgres://{user}:{password}@{host}:{port}/{database}
facts:        name version host port addr base_url scale arch image   (built-in, read-only)
live:         state instances started_at restarts   (after conditions only)
```

`fn render(record: &Record) -> String` is the testable part.

- [ ] **Step 1: Failing tests** — `render` on the Task 7 `PG` text asserts the `params:` block lines and the footer verbatim; `catalog.rs`: a v3 state literal with `manifest`/`verified`/object `dependencies` parses, and the v2 fixture (`tests/fixtures/state.sample.json`) still parses.
- [ ] **Step 2–4:** FAIL → implement → PASS. **Step 5:** `make check`.

---

# Part C — Migration and docs

### Task 10: Backfill script (`scripts/registry-backfill-records.mjs`)

**Files:**
- Create: `scripts/registry-backfill-records.mjs`

Reads the live root `state.json`, and for every package (skipping namespace `apps`, the folded alias) and every version entry: an image → download to a temp dir (or reuse the store path when `ply` reports it cached), `ply inspect <img> --json` → record; a stack → fetch its `.stack.toml`, `ply inspect <file> --json`; set `artifacts` from the state entry (`arch`, `src`, `sha256`, `bytes`, `verified: false`) grouping arches per version; POST `/api/publish/` with `backfill: true` and the owner's token from `~/.config/ply/credentials` (same file `ply push` reads), `--as <namespace>` semantics by sending `owner`. `--dry-run` prints the records; `--only ply/postgres` filters; failures are listed at the end and never stop the run. Idempotent: a version already holding a manifest is skipped (`GET` of its `.toml` URL returns 200 with the same text).

- [ ] Steps: write it; run `--dry-run` against the live state (read-only, allowed); paste the plan into the report. The real run is the owner's.

### Task 11: The deb/apk keg lane writes v3

**Files:**
- Modify: `scripts/registry-push.mjs` — beside each uploaded image, upload `{name}-{version}.toml` extracted with `ply inspect <img> --manifest`; version entries gain `manifest` (URL) and `verified: true`, and the derived fields from `ply inspect <img> --json` (`derive` equivalents in JS: volumes, links, publish from `[ports]`, dependencies, params); `index.json` regeneration includes the `.toml` names.
- Retire: `scripts/registry-v2-rebuild.mjs` (delete; its job is Task 10's).

- [ ] Steps: implement; `./scripts/registry-push.mjs --state-only --dry-run` (if the flag exists; otherwise add `--dry-run` handling to the state step) shows v3 entries; no upload.

### Task 12: Docs and the rollout checklist

**Files:**
- Modify: `docs/registries.md` (the read-side layout; self-hosting = serve the files; the push protocol; `--src`; `verified`), `docs/manifest.md` (`[package] owner/description/license/homepage`), `docs/cli.md` (`ply push` table from the spec; `ply inspect`), `docs/stacks.md` (publishing a stack unchanged in spirit, new URL shape)
- Modify: `TASKS.md` (git-ignored): `## Phase 14 — Registry: manifest is the record` with the rollout order: 1 registry release (`make release` in `app/`), 2 backfill run (`scripts/registry-backfill-records.mjs`), 3 CLI release 0.1.70, 4 run `registry-push.mjs --state-only` for the keg lane, 5 retire `registry/meta/*.json` and its merge step, 6 `DROP TABLE versions` once every record has a manifest (SQL prepared in the checklist), 7 smoke: `ply up iluxav/todos --plan` from a clean box and `ply inspect postgres@17`.

- [ ] Steps: write; every command/path in the docs verified against the code (`grep`); `make check` still green (docs don't compile; confirms nothing else moved).

---

## Self-review (done)

- **Spec coverage:** record shape → T1/T7; read-side layout + state v3 → T3/T9/T11; derived fields → T1 (server) with the page consuming them → T5; push protocol (upload/publish/legacy bridge, rules, `verified`) → T2/T4/T8; owner resolution → T4/T8 with fields from T6; stacks as records → T3/T7/T8; server storage → T2; migration (registry first, backfill, keg lane, retire meta, drop versions) → T10/T11/T12; `ply inspect` → T9; testing section → each task's tests; out of scope respected (no verifier, no `--out`, no lock changes).
- **Placeholder scan:** T7 Step 3, T9 Steps 2–4, T10–T12 steps are described rather than pasted; each names the exact functions, inputs, outputs, and commands. T7's `test_image_with_manifest` helper may need adding under `cfg(test)` in `image/squashfs.rs` — the task says so.
- **Type consistency:** `Artifact`/`PublishRecord`/`StoredRecord` (T1) used by T2–T5; `VersionEntry` (T3) mirrored by `RegistryVersion` (T5) and `ImageVersion` (T9); Rust `Record`/`Artifact` (T7) used by T8/T9/T10 (`--json`); `PackageKind` shared. `writeCatalogFiles` (T3) called from both routes (T4). `validatePublishBody` lives in `records.ts` (T4) and imports from `manifest.ts` (T1).
