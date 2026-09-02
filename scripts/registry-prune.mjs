#!/usr/bin/env node
// One-time (pre-launch) registry prune: delete pushed images that the cli
// tier would never have published — pure libraries nobody can declare.
// Keeps every `manual:` entry (bases, hand-pushed runtimes) and every
// package present in the given keep-catalogs.
//
//   ./scripts/apk-catalog.mjs --tier cli --arch x86_64  -o /tmp/cli-x64.json
//   ./scripts/apk-catalog.mjs --tier cli --arch aarch64 -o /tmp/cli-arm64.json
//   ./scripts/registry-prune.mjs --keep /tmp/cli-x64.json --keep /tmp/cli-arm64.json            # dry-run (plan only)
//   ./scripts/registry-prune.mjs --keep /tmp/cli-x64.json --keep /tmp/cli-arm64.json --delete   # do it
//   ./scripts/registry-republish.mjs                                                            # then refresh the catalog
//
// NEVER run while a registry-push batch is running (shared ledger).
// Post-launch this script should not exist: published versions are forever.

import { execFile } from "node:child_process";
import { promisify } from "node:util";
const execFileAsync = promisify(execFile);
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));

const MAX_IMAGE_BYTES = 100 * 2 ** 20; // hard ceiling; the catalog pre-rejects at 100 MB of apks // keep in sync with apk-catalog.mjs / registry-push.mjs

const args = { state: join(ROOT, "scripts/registry-state.json"), bucket: "ply-registry", keep: [], del: false, jobs: 6 };
const argv = process.argv.slice(2);
for (let i = 0; i < argv.length; i++) {
  const next = () => argv[++i];
  if (argv[i] === "--keep") args.keep.push(next());
  else if (argv[i] === "--state") args.state = next();
  else if (argv[i] === "--bucket") args.bucket = next();
  else if (argv[i] === "--jobs") args.jobs = parseInt(next(), 10);
  else if (argv[i] === "--delete") args.del = true;
  else { console.error(`unknown argument: ${argv[i]}`); process.exit(2); }
}
if (args.keep.length === 0) {
  console.error("no --keep catalogs given — refusing to classify everything as junk");
  process.exit(2);
}

// keep-set: package names per arch, from the cli catalogs
const keepNames = { x64: new Set(), arm64: new Set() };
for (const file of args.keep) {
  const cat = json(file);
  const arch = { x86_64: "x64", aarch64: "arm64" }[cat.arch];
  for (const p of cat.packages) keepNames[arch].add(p.name);
}
function json(f) { return JSON.parse(readFileSync(f, "utf8")); }

const state = json(args.state);
const saveState = () => writeFileSync(args.state, JSON.stringify(state, null, 1));

const junk = [];
let keepCount = 0, junkBytes = 0;
for (const [key, e] of Object.entries(state)) {
  if (!e.upload_path) { keepCount++; continue; } // size-capped: nothing on the CDN
  const arch = e.img.endsWith("-arm64.img") ? "arm64" : "x64";
  if (key.startsWith("manual:") || keepNames[arch].has(e.name)) { keepCount++; continue; }
  junk.push({ key, ...e });
  junkBytes += e.bytes ?? 0;
}
console.log(`ledger: ${Object.keys(state).length} entries — keep ${keepCount}, ` +
  `junk ${junk.length} (${(junkBytes / 2 ** 30).toFixed(2)} GiB)`);

if (!args.del) {
  for (const e of junk.slice(0, 20)) console.log(`  would delete ${e.upload_path}`);
  if (junk.length > 20) console.log(`  … and ${junk.length - 20} more`);
  console.log("dry-run — pass --delete to execute");
  process.exit(0);
}

// repos fully emptied by the prune lose their index.json too; survivors
// get theirs regenerated from the remaining ledger
const junkRepos = new Set(junk.map((e) => dirname(e.upload_path)));

function errLine(e) {
  const raw = (e.stderr?.toString() ?? e.message).replace(/\x1b\[[0-9;]*m/g, "");
  const lines = raw.split("\n").map((l) => l.trim()).filter(Boolean);
  return lines.find((l) => l.includes("[ERROR]")) ?? lines.at(-1) ?? "unknown error";
}
const rm = (path) =>
  execFileAsync("npx", ["wrangler", "r2", "object", "delete", `${args.bucket}/${path}`, "--remote"],
    { maxBuffer: 16 * 1024 * 1024 });

let done = 0, failed = 0, cursor = 0;
async function worker() {
  while (cursor < junk.length) {
    const e = junk[cursor++];
    try {
      await rm(e.upload_path);
      // An over-cap image stays in the ledger as a skipped record (no
      // upload_path): the catalog excludes it by real size and push never
      // retries it. Anything else is simply forgotten.
      if ((e.bytes ?? 0) > MAX_IMAGE_BYTES) {
        state[e.key] = {
          name: e.name, version: e.version, img: e.img, bytes: e.bytes,
          skipped: `image ${Math.round(e.bytes / 2 ** 20)} MiB > ${MAX_IMAGE_BYTES / 2 ** 20} MiB cap`,
          skipped_at: new Date().toISOString(),
        };
      } else {
        delete state[e.key];
      }
      saveState(); // ledger updated per object — crash-safe resume
      done++;
      if (done % 50 === 0) console.log(`  ${done}/${junk.length} deleted`);
    } catch (err) {
      failed++;
      console.log(`  ${e.upload_path} FAILED: ${errLine(err)}`);
    }
  }
}
await Promise.all(Array.from({ length: Math.min(args.jobs, junk.length) }, worker));

// index.json cleanup: regenerate for repos that still have entries, delete
// for repos that are now empty
const remainingRepos = new Map(); // repo -> [img…]
for (const e of Object.values(state)) {
  const repo = dirname(e.upload_path);
  if (!remainingRepos.has(repo)) remainingRepos.set(repo, []);
  remainingRepos.get(repo).push(e.img);
}
console.log(`cleaning up index.json for ${junkRepos.size} repos…`);
let indexed = 0, indexDeleted = 0;
const repoList = [...junkRepos];
let repoCursor = 0;
async function indexWorker(id) {
  while (repoCursor < repoList.length) {
    const repo = repoList[repoCursor++];
    try {
      if (remainingRepos.has(repo)) {
        const file = join(ROOT, `scripts/.prune-index-${id}.json`);
        writeFileSync(file, JSON.stringify(remainingRepos.get(repo).sort()));
        await execFileAsync("npx", ["wrangler", "r2", "object", "put",
          `${args.bucket}/${repo}/index.json`, "--file", file, "--remote",
          "--cache-control", "public, max-age=60", "--content-type", "application/json"],
          { maxBuffer: 16 * 1024 * 1024 });
        indexed++;
      } else {
        await rm(`${repo}/index.json`);
        indexDeleted++;
      }
      if ((indexed + indexDeleted) % 50 === 0)
        console.log(`  ${indexed + indexDeleted}/${repoList.length} indexes cleaned`);
    } catch (err) {
      console.log(`  index for ${repo} FAILED: ${errLine(err)}`);
    }
  }
}
await Promise.all(Array.from({ length: Math.min(args.jobs, repoList.length) }, (_, i) => indexWorker(i)));

console.log(`\npruned ${done} images (${failed} failed), ` +
  `${indexed} index.json regenerated, ${indexDeleted} index.json deleted`);
console.log("now refresh the catalog:  ./scripts/registry-republish.mjs");
