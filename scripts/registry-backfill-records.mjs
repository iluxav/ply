#!/usr/bin/env node
// Backfill: "the manifest is the record"
// (docs/superpowers/specs/2026-09-02-registry-manifest-truth-design.md).
//
// The registry seeds a manifest-less record (manifest_toml: "") at boot for
// every version that was stored by the old X-Ply-Meta push, so catalogs
// stay complete. This script fills each one in: for every published
// version it reads the ARTIFACT'S OWN embedded manifest with `ply inspect
// --json` (never the working-copy ply.toml — there isn't one here) and
// POSTs the record to /api/publish/ with backfill: true, which the
// endpoint accepts only while the version still has no manifest.
//
//   ./scripts/registry-backfill-records.mjs --dry-run
//   ./scripts/registry-backfill-records.mjs --dry-run --only ply/postgres
//   ./scripts/registry-backfill-records.mjs --dry-run --only ply/postgres@17.10.7 --store out/
//   ./scripts/registry-backfill-records.mjs --only ply/postgres         # the owner's real run
//   ./scripts/registry-backfill-records.mjs --force                     # re-check even versions that already have a manifest
//   ./scripts/registry-backfill-records.mjs --store DIR                 # reuse already-downloaded images/tomls (by filename) instead of fetching
//   ./scripts/registry-backfill-records.mjs --ply PATH                  # default: target/release/ply
//
// Reads only the live root state.json (read-only). Namespace `apps` is a
// folded alias of `ply` (the same packages, mirrored) and is skipped
// entirely — backfilling it would double-publish the same versions under
// the wrong owner. For every other package, versions are grouped by their
// `version` string (state.json carries one entry per arch); a version
// already holding a manifest (`GET`/`HEAD` of its `.toml` returns 200) is
// left alone unless --force is given.
//
// To read the manifest: an image version downloads its x64 entry (a stack
// has none — its own `.toml` is the artifact) and runs `ply inspect <file>
// --json`, which prints the Record exactly as `ply push --dry-run` would.
// `artifacts` in the published body come from the state entries, not from
// `inspect` (which never populates them): {arch, src, sha256, bytes,
// verified: false} for every arch this version has — except a stack, whose
// record always carries artifacts: [].
//
// --dry-run downloads nothing. It always prints the plan (process/skip per
// version); when --only narrows to specific version(s), it additionally
// shows the exact body that would be POSTed — but only by running `inspect`
// on a file --store already has; without --store it prints "(would
// download)" instead of fetching.
//
// A failure never stops the run — every version is attempted, failures are
// listed in the summary, and the script exits non-zero if any occurred. No
// secret ever appears in the output: the token is read but never printed,
// and a record carries none (a secret param stays `{secret: true}`, valueless).
//
// Requires: node (global fetch); a built `ply` for any run that actually
// inspects a file (skipped entirely by a bare `--dry-run` with no --only);
// a registry key at ~/.config/ply/credentials (the file `ply login`
// writes) or PLY_TOKEN, for a real (non-dry) run.

import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));
const CDN = "https://registry.plybox.sh";
const API = process.env.PLY_REGISTRY_API ?? "https://plybox.sh";
const FOLDED_ALIAS = "apps"; // mirrors ply/ 1:1 — never backfilled as its own namespace

// --- args ----------------------------------------------------------------
const args = {
  dryRun: false,
  force: false,
  only: null, // { owner, name, version: string | null }
  ply: join(ROOT, "target/release/ply"),
  store: null,
  keepTmp: false,
};

function parseOnly(spec) {
  const [ownerName, version] = spec.split("@");
  const [owner, name] = (ownerName ?? "").split("/");
  if (!owner || !name) {
    console.error(`--only ${spec}: expected owner/name[@version]`);
    process.exit(2);
  }
  return { owner, name, version: version || null };
}

const argv = process.argv.slice(2);
for (let i = 0; i < argv.length; i++) {
  const a = argv[i];
  if (a === "--dry-run") args.dryRun = true;
  else if (a === "--force") args.force = true;
  else if (a === "--only") args.only = parseOnly(argv[++i]);
  else if (a === "--ply") args.ply = argv[++i];
  else if (a === "--store") args.store = argv[++i];
  else if (a === "--keep-tmp") args.keepTmp = true;
  else {
    console.error(`unknown argument: ${a}`);
    process.exit(2);
  }
}

// --- credentials (same file `ply login`/`ply push` read) -----------------
function credentialsPath() {
  const base = process.env.XDG_CONFIG_HOME || join(homedir(), ".config");
  return join(base, "ply", "credentials");
}

function token() {
  if (process.env.PLY_TOKEN?.trim()) return process.env.PLY_TOKEN.trim();
  const path = credentialsPath();
  if (!existsSync(path)) return null;
  const raw = readFileSync(path, "utf8");
  const m = /^token\s*=\s*"([^"]*)"/m.exec(raw);
  return m?.[1] || null;
}

if (!args.dryRun) {
  if (!existsSync(args.ply)) {
    console.error(`no ply binary at ${args.ply} — build it (make static) or pass --ply PATH`);
    process.exit(2);
  }
  if (!token()) {
    console.error(`no key: set PLY_TOKEN, or \`ply login\` to write ${credentialsPath()}`);
    process.exit(2);
  }
}

// --- read the live catalog (read-only) ------------------------------------
const rootUrl = `${CDN}/state.json?t=${Date.now()}`;
console.log(`reading ${rootUrl}`);
const root = await fetch(rootUrl)
  .then((r) => r.json())
  .catch((e) => {
    console.error(`could not read ${rootUrl}: ${e}`);
    process.exit(1);
  });

const packages = (root.packages ?? []).filter((p) => p.namespace !== FOLDED_ALIAS);
const skippedAlias = (root.packages ?? []).length - packages.length;
console.log(
  `live catalog: ${root.packages?.length ?? 0} package(s), skipping ${skippedAlias} under \`${FOLDED_ALIAS}/\` (folded alias)`,
);

const packageSelected = (pkg) => !args.only || (pkg.namespace === args.only.owner && pkg.name === args.only.name);
const selected = packages.filter(packageSelected);
if (args.only && selected.length === 0) {
  console.error(`--only ${args.only.owner}/${args.only.name}: no such package in the live catalog`);
  process.exit(1);
}

/** state.json carries one entry per arch (and per version); group them. */
function versionGroups(pkg) {
  const byVersion = new Map();
  for (const v of pkg.versions ?? []) {
    if (!byVersion.has(v.version)) byVersion.set(v.version, []);
    byVersion.get(v.version).push(v);
  }
  return [...byVersion.entries()].map(([version, entries]) => ({ version, entries }));
}

const bareSha256 = (s) => (s ?? "").replace(/^sha256:/, "");

// --- local files: --store first (never fetched), else download -----------
function storeHas(url) {
  if (!args.store) return null;
  const base = decodeURIComponent(url.split("/").at(-1) ?? "");
  const path = join(args.store, base);
  return existsSync(path) ? path : null;
}

async function download(url, workdir) {
  const base = decodeURIComponent(url.split("/").at(-1) ?? "download");
  const dest = join(workdir, base);
  const res = await fetch(url);
  if (!res.ok) throw new Error(`GET ${url} → HTTP ${res.status}`);
  writeFileSync(dest, Buffer.from(await res.arrayBuffer()));
  return dest;
}

function runInspect(filePath) {
  const out = execFileSync(args.ply, ["inspect", filePath, "--json"], {
    maxBuffer: 8 * 1024 * 1024,
  });
  return JSON.parse(out.toString());
}

/**
 * The record for one version, plus its state-derived artifacts, as the
 * publish body (minus `backfill`, added by the caller). `allowDownload:
 * false` never touches the network for bytes — it uses --store or returns
 * null (the caller reports "would download").
 */
async function buildPublishBody(pkg, version, entries, { allowDownload, workdir }) {
  const owner = pkg.namespace;
  const name = pkg.name;
  const isStack = pkg.type === "stack";

  let srcUrl;
  if (isStack) {
    if (entries.length !== 1) {
      throw new Error(`stack has ${entries.length} state entries for this version — expected exactly 1`);
    }
    srcUrl = entries[0].src;
  } else {
    const x64 = entries.find((e) => e.arch === "x64");
    if (!x64) throw new Error("no x64 artifact for this version — nothing to inspect");
    srcUrl = x64.src;
  }
  if (!srcUrl) throw new Error("state entry has no src");

  let filePath = storeHas(srcUrl);
  if (!filePath) {
    if (!allowDownload) return null; // "(would download)"
    filePath = await download(srcUrl, workdir);
  }

  const record = runInspect(filePath);
  const artifacts = isStack
    ? []
    : entries.map((e) => ({
        arch: e.arch,
        src: e.src,
        sha256: bareSha256(e.sha256),
        bytes: e.bytes,
        verified: false,
      }));

  return {
    owner,
    name,
    version,
    type: record.type,
    manifest_toml: record.manifest_toml,
    manifest: record.manifest,
    artifacts,
    backfill: true,
  };
}

// --- per-version idempotency + dispatch -----------------------------------
let processed = 0; // published (real run) or "would process" (dry run)
let skipped = 0;
const failures = [];

async function hasManifestAlready(owner, name, version) {
  const url = `${CDN}/${owner}/${name}/${name}-${version}.toml`;
  const res = await fetch(url, { method: "HEAD" });
  return res.ok;
}

async function publish(body) {
  const key = token();
  const res = await fetch(`${API}/api/publish/`, {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  const respBody = await res.json().catch(() => ({}));
  return { status: res.status, ok: res.ok, body: respBody };
}

async function processVersion(pkg, version, entries, workdir) {
  const owner = pkg.namespace;
  const name = pkg.name;
  const label = `${owner}/${name}@${version}`;

  let exists;
  try {
    exists = await hasManifestAlready(owner, name, version);
  } catch (e) {
    console.log(`${label} → FAIL: checking manifest: ${e.message}`);
    failures.push({ label, error: `checking manifest: ${e.message}` });
    return;
  }

  if (exists && !args.force) {
    console.log(`${label} → skip (has manifest)`);
    skipped++;
    return;
  }

  if (args.dryRun) {
    if (!args.only) {
      console.log(`${label} → would process`);
      processed++;
      return;
    }
    // --only narrows to specific version(s): show the real body when
    // --store already has the file, else say so without fetching anything.
    try {
      const body = await buildPublishBody(pkg, version, entries, { allowDownload: false });
      if (body === null) {
        console.log(`${label} → would process (would download)`);
      } else {
        console.log(`${label} → would process; body:`);
        console.log(JSON.stringify(body, null, 1));
      }
      processed++;
    } catch (e) {
      console.log(`${label} → FAIL: ${e.message}`);
      failures.push({ label, error: e.message });
    }
    return;
  }

  // real run
  try {
    const body = await buildPublishBody(pkg, version, entries, { allowDownload: true, workdir });
    const res = await publish(body);
    if (res.ok) {
      console.log(`${label} → ${res.status}`);
      processed++;
    } else {
      const msg = res.body?.error ?? "no reason given";
      const diff = res.body?.diff ? `\n${res.body.diff}` : "";
      console.log(`${label} → FAIL: ${res.status} ${msg}${diff}`);
      failures.push({ label, error: `${res.status} ${msg}` });
    }
  } catch (e) {
    console.log(`${label} → FAIL: ${e.message}`);
    failures.push({ label, error: e.message });
  }
}

// --- run -------------------------------------------------------------------
const workdir = mkdtempSync(join(tmpdir(), "ply-backfill-"));
try {
  for (const pkg of selected) {
    for (const { version, entries } of versionGroups(pkg)) {
      if (args.only?.version && args.only.version !== version) continue;
      await processVersion(pkg, version, entries, workdir);
    }
  }
} finally {
  if (!args.keepTmp) rmSync(workdir, { recursive: true, force: true });
}

console.log(
  `\n${args.dryRun ? "would process" : "published"}: ${processed}, skipped: ${skipped}, failed: ${failures.length}`,
);
if (failures.length) {
  console.log("failures:");
  for (const f of failures) console.log(`  ${f.label}: ${f.error}`);
  process.exit(1);
}
