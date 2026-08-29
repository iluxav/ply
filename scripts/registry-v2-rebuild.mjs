#!/usr/bin/env node
// Rebuild the OFFICIAL registry catalogs (apps/ + ply/) to the v2 shape by
// enriching each version with metadata derived from its own image, then
// republishing state.json to the LIVE bucket.
//
//   ./scripts/registry-v2-rebuild.mjs --dry-run      # plan only, no writes
//   ./scripts/registry-v2-rebuild.mjs                # enrich apps/+ply/, upload
//   ./scripts/registry-v2-rebuild.mjs --only apps    # one namespace
//   ./scripts/registry-v2-rebuild.mjs --bucket X     # default: ply-registry-deb (LIVE)
//   ./scripts/registry-v2-rebuild.mjs --ply PATH     # default: target/release/ply
//
// What it does, per official package/version:
//   1. src  = a full URL   (https://registry.plybox.sh/<path>) — the v2 rule
//   2. type = derived from the image (`ply inspect`): app | layer
//   3. volumes / links / dependencies = derived from the image
//   `img` and the legacy `path` are KEPT alongside `src` so the current site
//   reader (which prefers src, falls back to path) never breaks.
//
// SOURCE OF TRUTH: it reads the live root state.json and rewrites ONLY the
// apps/ and ply/ namespaces. Every other namespace (iluxav/… — DB-backed) is
// preserved byte-for-byte, so a concurrent user push can't be clobbered. Those
// user namespaces are rebuilt separately — see USER NAMESPACES at the bottom.
//
// Idempotent: re-running re-derives from the same immutable images and writes
// the same result. Safe to interrupt and resume.
//
// Requires: node (global fetch), a built `ply` (for `ply inspect`), and
// `wrangler login` once (unless --dry-run).

import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));
const CDN = "https://registry.plybox.sh";
const OFFICIAL = new Set(["apps", "ply"]);

const args = {
  bucket: "ply-registry-deb", // the LIVE bucket (registry-push.mjs defaults to the OLD one)
  ply: join(ROOT, "target/release/ply"),
  dryRun: false,
  only: null, // "apps" | "ply"
  keepTmp: false,
};
const argv = process.argv.slice(2);
for (let i = 0; i < argv.length; i++) {
  const a = argv[i];
  if (a === "--dry-run") args.dryRun = true;
  else if (a === "--bucket") args.bucket = argv[++i];
  else if (a === "--ply") args.ply = argv[++i];
  else if (a === "--only") args.only = argv[++i];
  else if (a === "--keep-tmp") args.keepTmp = true;
  else {
    console.error(`unknown argument: ${a}`);
    process.exit(2);
  }
}

if (!args.dryRun && !existsSync(args.ply)) {
  console.error(`no ply binary at ${args.ply} — build it (make static) or pass --ply PATH`);
  process.exit(2);
}

// --- read the live catalog ----------------------------------------------------
const rootUrl = `${CDN}/state.json?t=${Date.now()}`;
const root = await (await fetch(rootUrl)).json().catch((e) => {
  console.error(`could not read ${rootUrl}: ${e}`);
  process.exit(1);
});
const allPackages = root.packages ?? [];

const wanted = (ns) => OFFICIAL.has(ns) && (!args.only || args.only === ns);
const official = allPackages.filter((p) => wanted(p.namespace));
const preserved = allPackages.filter((p) => !wanted(p.namespace));

console.log(`live catalog: ${allPackages.length} packages`);
console.log(`  rebuilding: ${official.length} (${[...new Set(official.map((p) => p.namespace))].join(", ") || "none"})`);
console.log(`  preserving: ${preserved.length} (${[...new Set(preserved.map((p) => p.namespace))].join(", ") || "none"})`);
if (official.length === 0) {
  console.log("nothing to rebuild for the selected namespace(s).");
  process.exit(0);
}

// --- derive metadata from an image via `ply inspect` --------------------------
const workdir = mkdtempSync(join(tmpdir(), "ply-v2-"));
let derivedCount = 0;
let failedCount = 0;

async function deriveMeta(src) {
  if (args.dryRun) return { type: undefined, volumes: [], links: [], dependencies: [], dry: true };
  const res = await fetch(src);
  if (!res.ok) throw new Error(`GET ${src} → HTTP ${res.status}`);
  const buf = Buffer.from(await res.arrayBuffer());
  const tmp = join(workdir, `img-${derivedCount + failedCount}.img`);
  writeFileSync(tmp, buf);
  try {
    const out = execFileSync(args.ply, ["inspect", tmp, "--json"], {
      maxBuffer: 8 * 1024 * 1024,
    }).toString();
    const m = JSON.parse(out);
    return {
      type: m.type ?? "layer",
      volumes: m.volumes ?? [],
      links: m.links ?? [],
      dependencies: m.dependencies ?? [],
    };
  } finally {
    if (!args.keepTmp) rmSync(tmp, { force: true });
  }
}

// --- rebuild each official package to v2 --------------------------------------
for (const pkg of official) {
  let pkgType = undefined;
  const versions = [];
  for (const v of pkg.versions ?? []) {
    if (!v.path) {
      console.log(`  ${pkg.namespace}/${pkg.name} ${v.version}: no path — skipping (URL/phantom version?)`);
      versions.push(v); // leave as-is; nothing to derive
      continue;
    }
    const src = `${CDN}/${v.path}`;
    const out = { version: v.version, img: v.img, arch: v.arch, src, path: v.path, bytes: v.bytes, pushed_at: v.pushed_at };
    try {
      const meta = await deriveMeta(src);
      if (!meta.dry) {
        derivedCount++;
        if (meta.type) pkgType = meta.type;
        if (meta.volumes.length) out.volumes = meta.volumes;
        if (meta.links.length) out.links = meta.links;
        if (meta.dependencies.length) out.dependencies = meta.dependencies;
      }
      console.log(
        `  ${pkg.namespace}/${pkg.name} ${v.version} (${v.arch}) → src${meta.dry ? "" : `, type=${meta.type}, vols=${meta.volumes.length}, deps=${meta.dependencies.length}`}`,
      );
    } catch (e) {
      failedCount++;
      console.log(`  ${pkg.namespace}/${pkg.name} ${v.version}: derive FAILED (${e.message}) — src set, no metadata`);
    }
    versions.push(out);
  }
  // package type: derived wins, else keep what the catalog already had, else
  // the namespace default (apps → app, ply → layer).
  pkg.type = pkgType ?? pkg.type ?? (pkg.namespace === "apps" ? "app" : "layer");
  pkg.versions = versions;
}

if (!args.keepTmp) rmSync(workdir, { recursive: true, force: true });
console.log(`\nderived ${derivedCount} version(s), ${failedCount} failed`);

// --- assemble + publish -------------------------------------------------------
const byNamespace = (pkgs, ns) => pkgs.filter((p) => p.namespace === ns);
const sortPkgs = (pkgs) =>
  [...pkgs].sort((a, b) =>
    a.namespace === b.namespace ? a.name.localeCompare(b.name) : a.namespace.localeCompare(b.namespace),
  );
const snapshot = (pkgs) => ({
  updated: new Date().toISOString().replace(/\.\d+Z$/, "Z"),
  package_count: pkgs.length,
  image_count: pkgs.reduce((n, p) => n + (p.versions?.length ?? 0), 0),
  total_bytes: pkgs.reduce((n, p) => n + (p.versions ?? []).reduce((m, v) => m + (v.bytes ?? 0), 0), 0),
  packages: pkgs,
});

// root = rebuilt official + preserved foreign (iluxav untouched)
const everything = sortPkgs([...official, ...preserved]);
const toPublish = [["state.json", snapshot(everything)]];
for (const ns of new Set(official.map((p) => p.namespace)))
  toPublish.push([`${ns}/state.json`, snapshot(sortPkgs(byNamespace(official, ns)))]);

if (args.dryRun) {
  console.log(`\n[dry run] would publish to bucket ${args.bucket}:`);
  for (const [key, obj] of toPublish)
    console.log(`  ${key}  (${obj.package_count} packages, ${obj.image_count} images)`);
  console.log("\n[dry run] no images downloaded, no metadata derived, nothing uploaded.");
  console.log("re-run without --dry-run (needs a built ply + wrangler login) to do it for real.");
  process.exit(0);
}

const pubDir = mkdtempSync(join(tmpdir(), "ply-v2-pub-"));
for (const [key, obj] of toPublish) {
  const file = join(pubDir, key.replace(/\//g, "__"));
  writeFileSync(file, JSON.stringify(obj, null, 1));
  execFileSync(
    "npx",
    ["wrangler", "r2", "object", "put", `${args.bucket}/${key}`, "--file", file, "--remote",
      "--cache-control", "public, max-age=300", "--content-type", "application/json"],
    { stdio: ["ignore", "inherit", "inherit"] },
  );
  console.log(`published ${key} (${obj.package_count} packages, ${obj.image_count} images)`);
}
rmSync(pubDir, { recursive: true, force: true });

console.log(`\ndone. Verify: curl -s ${CDN}/state.json | grep -c '"src"'`);

// ─── USER NAMESPACES (iluxav/…) — DB-backed, rebuilt separately ──────────────
//
// This script preserves them untouched (they'd be clobbered by the next user
// push otherwise, since the push route regenerates a namespace from the DB).
// For a couple of packages the simplest v2 path is wipe-and-re-push with the
// v2 CLI (append-only refuses a same-version re-push, so delete the rows first):
//
//   # 1. delete the namespace's rows (where POSTGRES_* reaches the DB):
//   #      DELETE FROM versions WHERE package_id IN
//   #        (SELECT id FROM packages WHERE owner='iluxav');
//   #      DELETE FROM packages WHERE owner='iluxav';
//   # 2. (optional) delete its R2 objects under iluxav/ so bytes match rows
//   # 3. re-push the local images — the v2 CLI sends X-Ply-Meta:
//   #      ply push ply-dashboard/dashboard-<ver>-linux-x64.img
//
// A re-push regenerates iluxav/state.json AND merges into root (preserving
// apps/+ply/, which this script owns), so the two mechanisms compose cleanly.
