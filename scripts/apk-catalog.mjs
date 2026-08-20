#!/usr/bin/env node
// Build the conversion catalog for the official ply registry.
//
// Downloads the latest APKINDEX for main+community, parses it, and emits a
// clean apk2pkg.json: one entry per package with its apk download URL, the
// canonical ply image filename it will become, and the R2 upload path.
//
// Usage:
//   ./scripts/apk-catalog.mjs [--branch v3.20] [--arch x86_64] [-o apk2pkg.json]
//                             [--tier all|core|main-core]
//
// Tiers:
//   all       every convertible package (default)
//   core      noise subpackages removed (-doc/-dev/-dbg/-lang/fonts/…)
//   main-core core ∩ Alpine's `main` repo — the mainstream ~13%, wave 1
//
// Name/version rules mirror apk2pkg exactly (keep them in sync):
//   name:    lowercase, ++ -> pp, + -> p, _ -> -
//   version: leading digits/dots of the apk version, padded to x.y.z

import { gunzipSync } from "node:zlib";
import { writeFileSync } from "node:fs";

const MIRROR = "https://dl-cdn.alpinelinux.org/alpine";
const ARCH_MAP = { x86_64: "x64", aarch64: "arm64" };

// --- tiny arg parser -------------------------------------------------------
const args = { branch: "v3.20", arch: "x86_64", repos: "main,community", output: "apk2pkg.json", tier: "all" };
const argv = process.argv.slice(2);
for (let i = 0; i < argv.length; i++) {
  const next = () => argv[++i];
  if (argv[i] === "--branch") args.branch = next();
  else if (argv[i] === "--arch") args.arch = next();
  else if (argv[i] === "--repos") args.repos = next();
  else if (argv[i] === "-o" || argv[i] === "--output") args.output = next();
  else if (argv[i] === "--tier") args.tier = next();
  else { console.error(`unknown argument: ${argv[i]}`); process.exit(2); }
}
if (!ARCH_MAP[args.arch]) { console.error(`unsupported arch: ${args.arch}`); process.exit(2); }

// --- rules (keep in sync with apk2pkg/src/main.rs) --------------------------
const sanitizeName = (n) =>
  n.toLowerCase().replaceAll("++", "pp").replaceAll("+", "p").replaceAll("_", "-");

function semverize(apkVersion) {
  const core = (apkVersion.match(/^[0-9.]*/)?.[0] ?? "").replace(/^\.+|\.+$/g, "");
  if (!core || !/^[0-9]/.test(core)) return null;
  const parts = core.split(".").concat(["0", "0", "0"]).slice(0, 3);
  if (parts.some((p) => p === "" || !/^[0-9]+$/.test(p))) return null;
  return parts.map((p) => String(parseInt(p, 10))).join(".");
}

// packaging shrapnel no app ever declares as a dependency
const NOISE = /-(doc|dev|dbg|static|openrc|lang|bash-completion|zsh-completion|fish-completion|pyc)$|^font-|-lang-/;

const validPlyName = (name) =>
  /^[a-z][a-z0-9._-]*$/.test(name) && !/-[0-9]/.test(name);

// --- minimal tar reader (just enough to find APKINDEX) ----------------------
function tarExtract(buf, wanted) {
  let off = 0;
  while (off + 512 <= buf.length) {
    const name = buf.toString("utf8", off, off + 100).replace(/\0.*$/, "");
    if (!name) break;
    const size = parseInt(buf.toString("utf8", off + 124, off + 136).trim(), 8) || 0;
    if (name === wanted) return buf.subarray(off + 512, off + 512 + size);
    off += 512 + Math.ceil(size / 512) * 512;
  }
  throw new Error(`${wanted} not found in tar`);
}

function* parseIndex(text) {
  for (const block of text.split("\n\n")) {
    const fields = {};
    for (const line of block.split("\n")) {
      if (line.length > 2 && line[1] === ":") fields[line[0]] = line.slice(2).trim();
    }
    if (fields.P && fields.V) yield fields;
  }
}

// --- main --------------------------------------------------------------------
const plyArch = ARCH_MAP[args.arch];
const packages = [];
const skipped = [];
const seen = new Set();

for (const repo of args.repos.split(",")) {
  const base = `${MIRROR}/${args.branch}/${repo}/${args.arch}`;
  const url = `${base}/APKINDEX.tar.gz`;
  console.error(`fetching ${url}`);
  const res = await fetch(url);
  if (!res.ok) { console.error(`  ${res.status} — skipping ${repo}`); continue; }
  const gz = Buffer.from(await res.arrayBuffer());
  const text = tarExtract(gunzipSync(gz), "APKINDEX").toString("utf8");

  for (const f of parseIndex(text)) {
    const apk = f.P, apkVersion = f.V;
    if (seen.has(apk)) continue; // main wins over community duplicates
    seen.add(apk);

    const name = sanitizeName(apk);
    const version = semverize(apkVersion);
    const problem =
      version === null ? `unversionable: ${apkVersion}`
      : !validPlyName(name) ? `invalid ply name: ${name}`
      : null;
    if (problem) {
      skipped.push({ apk, apk_version: apkVersion, repo, reason: problem });
      continue;
    }

    const img = `${name}-${version}-linux-${plyArch}.img`;
    packages.push({
      apk,
      apk_version: apkVersion,
      repo,
      name,
      version,
      apk_url: `${base}/${apk}-${apkVersion}.apk`,
      img,
      upload_path: `ply/${name}/${img}`,
      // APKINDEX metadata (sizes in bytes)
      size: parseInt(f.S ?? "0", 10),            // .apk download size
      installed_size: parseInt(f.I ?? "0", 10),  // unpacked size
      description: f.T ?? "",
      license: f.L ?? "",
      url: f.U ?? "",
      origin: f.o ?? apk,                        // source package (groups subpackages)
      maintainer: f.m ?? "",
      build_time: parseInt(f.t ?? "0", 10),
      depends: (f.D ?? "").split(/\s+/).filter(Boolean),
      provides: (f.p ?? "").split(/\s+/).filter(Boolean),
    });
  }
}

// reverse-dependency counts: how load-bearing each package is
const providerOf = {};
for (const p of packages) {
  providerOf[p.apk] = p.apk;
  for (const prov of p.provides) providerOf[prov.split("=")[0]] = p.apk;
}
const indegree = {};
for (const p of packages) {
  for (const d of p.depends) {
    const dep = d.split(/[<>=~]/)[0];
    const provider = providerOf[dep];
    if (provider) indegree[provider] = (indegree[provider] ?? 0) + 1;
  }
}
for (const p of packages) p.reverse_deps = indegree[p.apk] ?? 0;

// tier filter (after reverse_deps so counts reflect the whole universe)
let selected = packages;
if (args.tier === "core") selected = packages.filter((p) => !NOISE.test(p.apk));
else if (args.tier === "main-core")
  selected = packages.filter((p) => !NOISE.test(p.apk) && p.repo === "main");
else if (args.tier !== "all") { console.error(`unknown tier: ${args.tier}`); process.exit(2); }
selected.sort((a, b) => b.reverse_deps - a.reverse_deps); // load-bearing first

const catalog = {
  generated: new Date().toISOString().replace(/\.\d+Z$/, "Z"),
  tier: args.tier,
  branch: args.branch,
  arch: args.arch,
  mirror: MIRROR,
  package_count: selected.length,
  skipped_count: skipped.length,
  packages: selected,
  skipped,
};
writeFileSync(args.output, JSON.stringify(catalog, null, 1));
console.error(`${args.output}: ${selected.length} packages (tier: ${args.tier}), ${skipped.length} skipped`);
