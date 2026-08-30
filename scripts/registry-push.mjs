#!/usr/bin/env node
// Convert Alpine packages and push them to the official ply registry on R2.
//
//   ./scripts/registry-push.mjs --limit 5                 # next 5 unprocessed
//   ./scripts/registry-push.mjs --only jq,ffmpeg          # specific packages
//   ./scripts/registry-push.mjs --dry-run                 # show the plan only
//   ./scripts/registry-push.mjs --jobs 8                  # parallel workers (default 4)
//   ./scripts/registry-push.mjs --reindex                 # regen ALL index.json from ledger
//   ./scripts/registry-push.mjs --state-only              # republish state.json + page only
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

import { execFile, execFileSync } from "node:child_process";
import { promisify } from "node:util";
const execFileAsync = promisify(execFile);
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync, rmSync, statSync } from "node:fs";
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
// The apk catalog only drives batch conversion; --file and --state-only
// runs work without it (the alpine lane is retired).
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

if (args.stateOnly) {
  await publishState();
  process.exit(0);
}
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

// v2 catalog metadata, derived from the image itself via `ply inspect`
// (client-derives, server-stores). Best-effort: a missing/old `ply` binary
// leaves the entry without volumes/deps — `src` still comes from the path.
async function deriveMeta(imgPath) {
  try {
    const { stdout } = await execFileAsync(args.ply, ["inspect", imgPath, "--json"],
      { maxBuffer: 8 * 1024 * 1024 });
    const m = JSON.parse(stdout);
    return {
      type: m.type ?? "layer",
      volumes: m.volumes ?? [],
      links: m.links ?? [],
      dependencies: m.dependencies ?? [],
    };
  } catch (e) {
    console.log(`  (meta derive failed for ${imgPath.split("/").at(-1)}: ${errLine(e)}) — no v2 metadata`);
    return null;
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

    // 3. ledger (written after every success — crash-safe deltas; JS is
    // single-threaded, so concurrent tasks can't interleave inside this block)
    const derived = await deriveMeta(img);
    state[ledgerKey(p)] = {
      name: p.name,
      version: p.version,
      img: p.img,
      upload_path: p.upload_path,
      bytes,
      pushed_at: new Date().toISOString(),
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
    state[key] = {
      name: p.name,
      version: p.version,
      img: p.img,
      upload_path: p.upload_path,
      bytes: statSync(p.file).size,
      pushed_at: new Date().toISOString(),
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
  const imgs = Object.values(state)
    .filter((e) => e.upload_path && dirname(e.upload_path) === repoDir).map((e) => e.img).sort();
  const indexFile = join(workdir, `index-${repoDir.replaceAll("/", "-")}.json`);
  writeFileSync(indexFile, JSON.stringify(imgs));
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

if (reposToIndex.size > 0) {
  try {
    await publishState();
  } catch (e) {
    console.log(`publishState FAILED: ${e.message} — rerun with --state-only`);
    process.exitCode = 1;
  }
}

console.log(`\ndone: ${ok} pushed, ${failed} failed, ` +
  `${reposToIndex.size - indexFailed}/${reposToIndex.size} index.json updated`);
console.log(`ledger: ${args.state} (${Object.keys(state).length} total)`);
if (indexFailed > 0) {
  console.log(`${indexFailed} index.json uploads failed — rerun with --reindex`);
  process.exitCode = 1;
}

// --- state.json: the machine-readable registry snapshot, served by the CDN ------
// The registry web page (web/registry/index.html) fetches /state.json and
// renders it. Sizes come from the ledger; entries pushed before sizes were
// recorded (or by hand) get a one-time HEAD against the CDN, cached back
// into the ledger.
async function publishState() {
  const entries = Object.values(state).filter((e) => e.upload_path);

  for (const e of entries) {
    if (e.bytes) continue;
    try {
      const res = await fetch(`https://registry.plybox.sh/${e.upload_path}`, { method: "HEAD" });
      if (res.ok) { e.bytes = parseInt(res.headers.get("content-length") ?? "0", 10); saveState(); }
    } catch { /* size stays unknown; page shows a dash */ }
  }

  const meta = new Map(catalog.packages.map((p) => [p.name, p]));
  // curated metadata: registry/meta/<owner>/<name>.json — the catalog is
  // code-reviewed; type/contract/origin come from git, not from a UI build
  const curated = new Map();
  const metaRoot = join(ROOT, "registry/meta");
  if (existsSync(metaRoot)) {
    for (const owner of readdirSync(metaRoot)) {
      const ownerDir = join(metaRoot, owner);
      if (!statSync(ownerDir).isDirectory()) continue;
      for (const f of readdirSync(ownerDir)) {
        if (!f.endsWith(".json")) continue;
        curated.set(f.slice(0, -5), { owner, ...JSON.parse(readFileSync(join(ownerDir, f), "utf8")) });
      }
    }
  }
  const byKey = new Map(); // "namespace/name" -> package record
  const seenImg = new Set();
  for (const e of entries) {
    if (seenImg.has(e.upload_path)) continue; // manual + apk entry for the same image
    seenImg.add(e.upload_path);
    const namespace = e.upload_path.split("/")[0];
    const key = `${namespace}/${e.name}`;
    if (!byKey.has(key)) {
      const m = meta.get(e.name);
      const c = curated.get(e.name) ?? {};
      byKey.set(key, {
        namespace,
        owner: c.owner ?? "ply",
        name: e.name,
        // type is derived from the image (client-derives); a curated override
        // still wins, then the namespace default for pre-derive ledger rows.
        type: e.type ?? c.type ?? (namespace === "apps" ? "app" : "layer"),
        description: c.description ?? m?.description ?? "",
        license: c.license ?? m?.license ?? "",
        homepage: c.homepage ?? m?.url ?? "",
        contract: c.contract,
        publish: c.publish,
        grant_links: c.grant_links,
        origin: c.origin,
        // Provenance: unmodified Alpine build. repo/apk/origin locate the
        // package page and the aports recipe (license text, source tarball).
        alpine: m ? { branch: catalog.branch, repo: m.repo, apk: m.apk, origin: m.origin } : undefined,
        versions: [],
      });
    }
    byKey.get(key).versions.push({
      version: e.version,
      img: e.img,
      arch: e.img.endsWith("-arm64.img") ? "arm64" : "x64",
      // src is the v2 canonical location — a full URL. path is kept for the
      // current site reader until the whole catalog is on src.
      src: `https://registry.plybox.sh/${e.upload_path}`,
      path: e.upload_path,
      bytes: e.bytes ?? 0,
      pushed_at: e.pushed_at,
      ...(e.volumes?.length ? { volumes: e.volumes } : {}),
      ...(e.links?.length ? { links: e.links } : {}),
      ...(e.dependencies?.length ? { dependencies: e.dependencies } : {}),
    });
  }
  const packages = [...byKey.values()].sort((a, b) =>
    a.namespace === b.namespace ? a.name.localeCompare(b.name) : a.namespace.localeCompare(b.namespace));
  for (const p of packages)
    p.versions.sort((a, b) => a.version.localeCompare(b.version, undefined, { numeric: true }));

  const snapshot = (pkgs) => ({
    updated: new Date().toISOString().replace(/\.\d+Z$/, "Z"),
    package_count: pkgs.length,
    image_count: pkgs.reduce((n, p) => n + p.versions.length, 0),
    total_bytes: pkgs.reduce((n, p) => n + p.versions.reduce((m, v) => m + v.bytes, 0), 0),
    packages: pkgs,
  });

  const dir = join(ROOT, "scripts/.push-work");
  mkdirSync(dir, { recursive: true });
  const publish = (key, obj) => {
    const file = join(dir, key.replace(/\//g, "__"));
    writeFileSync(file, JSON.stringify(obj, null, 1));
    execFileSync("npx", ["wrangler", "r2", "object", "put",
      `${args.bucket}/${key}`, "--file", file, "--remote",
      "--cache-control", "public, max-age=30", "--content-type", "application/json"],
      { stdio: ["ignore", "ignore", "pipe"] });
    rmSync(file, { force: true });
    console.log(`${key} published (${obj.package_count} packages, ${obj.image_count} images)`);
  };

  // Root: the website's view of everything. Community namespaces are
  // pushed through the site's /api/push/ and own their slice of this
  // file — preserve every namespace the ledger doesn't know. The
  // cache-buster query skips the CDN so a minutes-old push can't be lost.
  let foreign = [];
  try {
    const cur = await (await fetch(`https://registry.plybox.sh/state.json?t=${Date.now()}`)).json();
    const mine = new Set(packages.map((p) => p.namespace));
    foreign = (cur.packages ?? []).filter((p) => p.namespace && !mine.has(p.namespace));
  } catch (e) {
    console.log(`WARN: could not read current state.json (${e.message}) — community entries may be dropped this publish`);
  }
  const everything = [...packages, ...foreign].sort((a, b) =>
    a.namespace === b.namespace ? a.name.localeCompare(b.name) : a.namespace.localeCompare(b.namespace));
  publish("state.json", snapshot(everything));
  // Per namespace: the catalog `ply search` / `ply add` read at the source
  // prefix (https://registry.plybox.sh/<ns>/state.json).
  for (const ns of new Set(packages.map((p) => p.namespace)))
    publish(`${ns}/state.json`, snapshot(packages.filter((p) => p.namespace === ns)));

  // The browse UI lives at plybox.sh/registry now — the bucket's root page
  // is a permanent redirect so old links keep working.
  const page = join(dir, "registry-index.html");
  writeFileSync(page, `<!doctype html><meta charset="utf-8">
<meta http-equiv="refresh" content="0; url=https://plybox.sh/registry/">
<title>ply registry</title>
<a href="https://plybox.sh/registry/">browse the registry at plybox.sh/registry</a>
`);
  execFileSync("npx", ["wrangler", "r2", "object", "put",
    `${args.bucket}/index.html`, "--file", page, "--remote",
    "--cache-control", "public, max-age=300", "--content-type", "text/html"],
    { stdio: ["ignore", "ignore", "pipe"] });
  rmSync(page, { force: true });
  console.log("registry index.html re-rendered and published");
}
