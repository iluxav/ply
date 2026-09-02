#!/usr/bin/env node
// Convert Alpine packages and push them to the official ply registry on R2.
//
//   ./scripts/registry-push.mjs --limit 5                 # next 5 unprocessed
//   ./scripts/registry-push.mjs --only jq,ffmpeg          # specific packages
//   ./scripts/registry-push.mjs --dry-run                 # show the plan only
//   ./scripts/registry-push.mjs --jobs 8                  # parallel workers (default 4)
//   ./scripts/registry-push.mjs --reindex                 # regen ALL index.json from ledger
//   ./scripts/registry-push.mjs --file out/foo-1.2.3-linux-arm64.img [--file …]
//                                                         # push prebuilt image(s) as-is
//                                                         # (--namespace ply is the default)
//
// One batch at a time: two concurrent runs would race on the ledger file
// and lose entries.
//
// Reads the catalog (scripts/apk2pkg.json — regenerate with apk-catalog.mjs),
// keeps a local ledger (scripts/registry-state.json) of what was already
// converted+pushed, and only processes the delta. The nightly job is:
//   ./scripts/apk-catalog.mjs --tier main-core -o scripts/apk2pkg.json
//   ./scripts/registry-push.mjs --limit 100000
//
// Uploads use wrangler (`wrangler login` once). Each repo's index.json is
// regenerated from the ledger after the batch.
//
// This lane writes BYTES ONLY — the image, its `<name>-<version>.toml`, and
// the package's index.json. It does NOT write state.json, and must not: the
// catalog is derived from the registry's `records` table, which holds every
// namespace (ply/postgres, notify, pg-backup, dashboard, plybox-web, the
// community's). A snapshot rendered from this 8-package deb ledger would
// replace all of it. The keg lane's path into the catalog is
// `scripts/registry-republish.mjs` → the registry API.

import { execFile } from "node:child_process";
import { promisify } from "node:util";
const execFileAsync = promisify(execFile);
import { existsSync, mkdirSync, readFileSync, writeFileSync, rmSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));

// --- args --------------------------------------------------------------------
const args = {
  catalog: join(ROOT, "scripts/apk2pkg.json"),
  state: join(ROOT, "scripts/registry-state.json"),
  bucket: "ply-registry",
  limit: 5,
  jobs: 4,
  only: null,
  dryRun: false,
  apk2pkg: join(ROOT, "target/release/apk2pkg"),
  ply: join(ROOT, "target/release/ply"),
  files: [],
  namespace: "ply",
};
// Hard cap on what this registry serves (keep in sync with apk-catalog.mjs):
// enforced on converted images and on --file pushes alike.
const MAX_IMAGE_BYTES = 100 * 2 ** 20; // hard ceiling; the catalog pre-rejects at 100 MB of apks

const argv = process.argv.slice(2);
for (let i = 0; i < argv.length; i++) {
  const next = () => argv[++i];
  if (argv[i] === "--catalog") args.catalog = next();
  else if (argv[i] === "--state") args.state = next();
  else if (argv[i] === "--bucket") args.bucket = next();
  else if (argv[i] === "--limit") args.limit = parseInt(next(), 10);
  else if (argv[i] === "--jobs") args.jobs = parseInt(next(), 10);
  else if (argv[i] === "--only") args.only = next().split(",");
  else if (argv[i] === "--dry-run") args.dryRun = true;
  else if (argv[i] === "--state-only") args.stateOnly = true;
  else if (argv[i] === "--reindex") args.reindex = true;
  else if (argv[i] === "--apk2pkg") args.apk2pkg = next();
  else if (argv[i] === "--ply") args.ply = next();
  else if (argv[i] === "--file") args.files.push(next());
  else if (argv[i] === "--namespace") args.namespace = next();
  else { console.error(`unknown argument: ${argv[i]}`); process.exit(2); }
}

// --state-only is gone, and cannot be quietly ignored: the runbook used to
// end with it, and running it now would delete most of the catalog.
if (args.stateOnly) {
  console.error(
    "--state-only was removed: this lane no longer writes state.json.\n" +
    "The catalog is derived from the registry's records table — every namespace,\n" +
    "not just this ledger's kegs — so a snapshot rendered here would delete\n" +
    "ply/postgres, notify, pg-backup, dashboard and plybox-web, and regress redis.\n" +
    "To republish keg metadata into the catalog, go through the API:\n" +
    "  ./scripts/registry-republish.mjs",
  );
  process.exit(2);
}

// --file mode: parse canonical filenames up front so a typo dies before any upload
const manualPushes = args.files.map((file) => {
  const base = file.split("/").at(-1);
  const m = base.match(/^(.+?)-(\d+\.\d+\.\d+)-(linux)-(x64|arm64)\.img$/);
  if (!m) {
    console.error(`--file ${file}: name must be <name>-<x.y.z>-linux-<x64|arm64>.img`);
    process.exit(2);
  }
  if (!existsSync(file)) { console.error(`--file ${file}: no such file`); process.exit(2); }
  const fileBytes = statSync(file).size;
  if (fileBytes > MAX_IMAGE_BYTES) {
    console.error(`--file ${file}: ${Math.round(fileBytes / 2 ** 20)} MiB exceeds the ${MAX_IMAGE_BYTES / 2 ** 20} MiB image cap — not pushing`);
    process.exit(2);
  }
  return { file, img: base, name: m[1], version: m[2],
           upload_path: `${args.namespace}/${m[1]}/${base}` };
});

// --- load catalog + ledger ----------------------------------------------------
// The apk catalog only drives batch conversion; a --file run works without
// it (the alpine lane is retired).
const catalog = existsSync(args.catalog)
  ? JSON.parse(readFileSync(args.catalog, "utf8"))
  : { package_count: 0, packages: [], arch: "x86_64", tier: "none", branch: "-" };
const state = existsSync(args.state) ? JSON.parse(readFileSync(args.state, "utf8")) : {};
const saveState = () => writeFileSync(args.state, JSON.stringify(state, null, 1));

// Keys carry the arch (apk@version:arm64); entries from before the arm64
// era have bare keys — those were all x64 pushes, honored as such.
const catalogArch = { x86_64: "x64", aarch64: "arm64" }[catalog.arch] ?? "x64";
const ledgerKey = (p) => `${p.apk}@${p.apk_version}:${catalogArch}`;
// Done = uploaded, or recorded as too large under a cap the image still
// exceeds. A skipped record whose size now fits (cap raised) is retried.
const settled = (e) => e && (e.upload_path || (e.skipped && (e.bytes ?? 0) > MAX_IMAGE_BYTES));
const alreadyPushed = (p) =>
  settled(state[ledgerKey(p)]) || (catalogArch === "x64" && settled(state[`${p.apk}@${p.apk_version}`]));

let todo = catalog.packages.filter((p) => !alreadyPushed(p));
if (args.only) todo = todo.filter((p) => args.only.includes(p.apk) || args.only.includes(p.name));
todo = todo.slice(0, args.limit);
if (manualPushes.length > 0) todo = []; // --file mode replaces the catalog batch

console.log(`catalog: ${catalog.package_count} (tier ${catalog.tier ?? "all"}, ${catalog.branch}/${catalog.arch})`);
console.log(
  `ledger:  ${catalog.packages.filter(alreadyPushed).length}/${catalog.packages.length} of this catalog already pushed ` +
  `(${Object.keys(state).length} ledger entries total, all arches)`,
);
console.log(`batch:   ${todo.length} to convert+push${args.dryRun ? " (dry run)" : ""}\n`);

if (args.dryRun) {
  for (const p of todo) console.log(`  would process ${p.apk}@${p.apk_version} -> ${p.upload_path}`);
  for (const p of manualPushes) console.log(`  would push ${p.file} -> ${p.upload_path}`);
  process.exit(0);
}
if (todo.length === 0 && manualPushes.length === 0 && !args.reindex) {
  console.log("nothing to do — registry is up to date with the catalog");
  process.exit(0);
}

// --- convert + upload (worker pool: each package is mostly network I/O) --------
const workdir = join(ROOT, "scripts/.push-work");
mkdirSync(workdir, { recursive: true });
const touchedRepos = new Set();
let ok = 0, failed = 0, tooBig = 0, cursor = 0;


// wrangler buries the real error mid-stderr behind ANSI color codes;
// strip them and prefer the [ERROR] line
function errLine(e) {
  const raw = (e.stderr?.toString() ?? e.message).replace(/\x1b\[[0-9;]*m/g, "");
  const lines = raw.split("\n").map((l) => l.trim()).filter(Boolean);
  return lines.find((l) => l.includes("[ERROR]")) ?? lines.at(-1) ?? "unknown error";
}

// v3: the manifest is the record. `table`/`str`/`derive` reproduce
// app/lib/manifest.ts::derive field-for-field, so this static-bucket lane
// (R2 + a hand-kept ledger, never the registry API) and the DB-backed one
// agree on what a manifest means.
const table = (m, key) => {
  const v = m?.[key];
  return v && typeof v === "object" && !Array.isArray(v) ? v : {};
};
const str = (v) => (typeof v === "string" ? v : "");

function derive(m) {
  const pkg = table(m, "package");
  const volumes = Object.values(table(m, "volumes"))
    .map((v) => str(v?.path))
    .filter(Boolean);
  const linksRaw = table(m, "requests").links;
  const links = Array.isArray(linksRaw)
    ? linksRaw.map((l) => (typeof l === "string" ? l : `${str(l?.host)}:${str(l?.at)}`))
    : [];
  const ports = Object.values(table(m, "ports"));
  const publish = ports.length === 1 && typeof ports[0] === "number" ? `internal:${ports[0]}` : undefined;
  const dependencies = {};
  for (const [k, v] of Object.entries(table(m, "dependencies"))) {
    dependencies[k] = typeof v === "string" ? v : str(v?.version);
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

// v3 metadata, derived from the image itself via `ply inspect --json`
// (client-derives, server-stores — this lane never used the registry API).
// Best-effort: a missing/old `ply` binary leaves the entry without v3
// fields — `src` still comes from the path.
async function deriveMeta(imgPath) {
  try {
    const { stdout } = await execFileAsync(args.ply, ["inspect", imgPath, "--json"],
      { maxBuffer: 8 * 1024 * 1024 });
    const record = JSON.parse(stdout);
    return { type: record.type ?? "layer", ...derive(record.manifest ?? {}) };
  } catch (e) {
    console.log(`  (meta derive failed for ${imgPath.split("/").at(-1)}: ${errLine(e)}) — no v3 metadata`);
    return null;
  }
}

// v3: upload the manifest .toml beside the image — arch-less
// (`{name}-{version}.toml`, the same URL for every arch of a version, per
// the registry's read-side file layout). Best-effort on both extraction and
// upload: a failure leaves the ledger entry without `manifest_toml_path`, so
// nothing downstream points a `manifest` URL at bytes that were never written.
async function uploadManifestToml(imgPath, uploadDir, name, version) {
  let text;
  try {
    const { stdout } = await execFileAsync(args.ply, ["inspect", imgPath, "--manifest"],
      { maxBuffer: 8 * 1024 * 1024 });
    // strip exactly the trailing newline `println!` adds, so the uploaded
    // bytes are the embedded manifest text verbatim
    text = stdout.endsWith("\n") ? stdout.slice(0, -1) : stdout;
  } catch (e) {
    console.log(`  (.toml extract failed for ${name}@${version}: ${errLine(e)}) — no manifest uploaded`);
    return null;
  }
  const uploadPath = `${uploadDir}/${name}-${version}.toml`;
  const tmp = join(workdir, `manifest-${name}-${version}.toml`);
  writeFileSync(tmp, text);
  try {
    await execFileAsync("npx", ["wrangler", "r2", "object", "put",
      `${args.bucket}/${uploadPath}`, "--file", tmp, "--remote",
      "--cache-control", "public, max-age=31536000, immutable",
      "--content-type", "application/toml"],
      { maxBuffer: 16 * 1024 * 1024 });
    return uploadPath;
  } catch (e) {
    console.log(`  (.toml upload failed for ${name}@${version}: ${errLine(e)}) — no manifest URL recorded`);
    return null;
  } finally {
    rmSync(tmp, { force: true });
  }
}

async function processOne(p) {
  const t0 = Date.now();
  console.log(`  converting ${p.apk}@${p.apk_version}…`);
  // private subdir per task — parallel apk2pkg runs must not share an output dir
  const dir = join(workdir, `job-${p.name}-${p.version}`);
  mkdirSync(dir, { recursive: true });
  try {
    // 1. convert (apk2pkg downloads the apk closure itself)
    await execFileAsync(args.apk2pkg,
      [p.apk, "--alpine", catalog.branch.replace(/^v/, ""), "--arch", catalogArch, "-o", dir],
      { maxBuffer: 16 * 1024 * 1024 });
    const img = join(dir, p.img);
    if (!existsSync(img)) throw new Error(`converter did not produce ${p.img}`);

    // Size cap (the catalog only estimates; this is the real number). A
    // too-large image is recorded in the ledger without an upload_path so
    // the next catalog excludes it and the next batch does not retry it.
    const bytes = statSync(img).size;
    if (bytes > MAX_IMAGE_BYTES) {
      state[ledgerKey(p)] = {
        name: p.name, version: p.version, img: p.img, bytes,
        skipped: `image ${Math.round(bytes / 2 ** 20)} MiB > ${MAX_IMAGE_BYTES / 2 ** 20} MiB cap`,
        skipped_at: new Date().toISOString(),
      };
      saveState();
      tooBig++;
      console.log(`[${ok + failed + tooBig}/${todo.length}] ${p.apk}@${p.apk_version} SKIPPED: ${state[ledgerKey(p)].skipped}`);
      return;
    }

    // 2. upload, immutable-cached (content-named — the bytes never change)
    await execFileAsync("npx", ["wrangler", "r2", "object", "put",
      `${args.bucket}/${p.upload_path}`, "--file", img, "--remote",
      "--cache-control", "public, max-age=31536000, immutable",
      "--content-type", "application/octet-stream"],
      { maxBuffer: 16 * 1024 * 1024 });

    // 3. v3: derive metadata from the image itself and upload its manifest
    // .toml beside it — both best-effort (see the two functions above).
    const derived = await deriveMeta(img);
    const manifestTomlPath = await uploadManifestToml(img, dirname(p.upload_path), p.name, p.version);

    // 4. ledger (written after every success — crash-safe deltas; JS is
    // single-threaded, so concurrent tasks can't interleave inside this block)
    state[ledgerKey(p)] = {
      name: p.name,
      version: p.version,
      img: p.img,
      upload_path: p.upload_path,
      bytes,
      pushed_at: new Date().toISOString(),
      verified: true,
      ...(manifestTomlPath ? { manifest_toml_path: manifestTomlPath } : {}),
      ...(derived ?? {}),
    };
    saveState();
    touchedRepos.add(dirname(p.upload_path)); // repo dir on the CDN, e.g. ply/jq
    ok++;
    console.log(`[${ok + failed + tooBig}/${todo.length}] ${p.apk}@${p.apk_version} ok (${((Date.now() - t0) / 1000).toFixed(1)}s)`);
  } catch (e) {
    failed++;
    const msg = errLine(e);
    console.log(`[${ok + failed + tooBig}/${todo.length}] ${p.apk}@${p.apk_version} FAILED: ${msg}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

async function worker() {
  while (cursor < todo.length) await processOne(todo[cursor++]);
}
await Promise.all(Array.from({ length: Math.min(args.jobs, todo.length) }, worker));

// --- push prebuilt images (--file mode) ----------------------------------------
// Append-only: refuse to replace a published image — a version's bytes never
// change; new bytes mean a new version (or a new arch, which is a new file).
for (const p of manualPushes) {
  // Keyed by upload_path, not filename: the same name-version can exist in
  // two namespaces (keg `ply/redis`, runnable app `apps/redis`) and must not
  // clobber each other's ledger entries.
  const key = `manual:${p.upload_path}`;
  if (Object.values(state).some((e) => e.upload_path === p.upload_path)) {
    failed++;
    console.log(`${p.img} FAILED: already published (append-only) — bump the version instead`);
    continue;
  }
  try {
    await execFileAsync("npx", ["wrangler", "r2", "object", "put",
      `${args.bucket}/${p.upload_path}`, "--file", p.file, "--remote",
      "--cache-control", "public, max-age=31536000, immutable",
      "--content-type", "application/octet-stream"],
      { maxBuffer: 16 * 1024 * 1024 });
    const derived = await deriveMeta(p.file);
    const manifestTomlPath = await uploadManifestToml(p.file, dirname(p.upload_path), p.name, p.version);
    state[key] = {
      name: p.name,
      version: p.version,
      img: p.img,
      upload_path: p.upload_path,
      bytes: statSync(p.file).size,
      pushed_at: new Date().toISOString(),
      verified: true,
      ...(manifestTomlPath ? { manifest_toml_path: manifestTomlPath } : {}),
      ...(derived ?? {}),
    };
    saveState();
    touchedRepos.add(dirname(p.upload_path));
    ok++;
    console.log(`pushed ${p.img} -> ${p.upload_path}`);
  } catch (e) {
    failed++;
    console.log(`${p.img} FAILED: ${errLine(e)}`);
  }
}

// --- regenerate index.json for every repo we touched ---------------------------
// (--reindex: every repo in the ledger — the recovery path after a crash here).
// One failed upload must not abort the rest; failures are reported and the
// repo is picked up by the next `--reindex`.
const reposToIndex = args.reindex
  ? new Set(Object.values(state).filter((e) => e.upload_path).map((e) => dirname(e.upload_path)))
  : touchedRepos;
let indexFailed = 0;
for (const repoDir of reposToIndex) {
  const repoEntries = Object.values(state)
    .filter((e) => e.upload_path && dirname(e.upload_path) === repoDir);
  const imgs = repoEntries.map((e) => e.img).sort();
  // v3: the manifest .toml beside each image — one per version, so dedupe
  // across arches that share it.
  const tomls = [...new Set(
    repoEntries.filter((e) => e.manifest_toml_path).map((e) => e.manifest_toml_path.split("/").at(-1)),
  )].sort();
  const indexFile = join(workdir, `index-${repoDir.replaceAll("/", "-")}.json`);
  writeFileSync(indexFile, JSON.stringify([...imgs, ...tomls]));
  try {
    await execFileAsync("npx", ["wrangler", "r2", "object", "put",
      `${args.bucket}/${repoDir}/index.json`, "--file", indexFile, "--remote",
      "--cache-control", "public, max-age=60",
      "--content-type", "application/json"],
      { maxBuffer: 16 * 1024 * 1024 });
  } catch (e) {
    indexFailed++;
    const msg = errLine(e);
    console.log(`index ${repoDir} FAILED: ${msg}`);
  }
  rmSync(indexFile, { force: true });
}

console.log(`\ndone: ${ok} pushed, ${failed} failed, ` +
  `${reposToIndex.size - indexFailed}/${reposToIndex.size} index.json updated`);
console.log(`ledger: ${args.state} (${Object.keys(state).length} total)`);
if (indexFailed > 0) {
  console.log(`${indexFailed} index.json uploads failed — rerun with --reindex`);
  process.exitCode = 1;
}
if (ok > 0) {
  console.log("bytes are up; the catalog is not — publish these versions into it with:");
  console.log("  ./scripts/registry-republish.mjs");
}
