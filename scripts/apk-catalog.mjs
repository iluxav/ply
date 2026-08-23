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
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));

const MIRROR = "https://dl-cdn.alpinelinux.org/alpine";
const ARCH_MAP = { x86_64: "x64", aarch64: "arm64" };

// --- tiny arg parser -------------------------------------------------------
const args = { branch: "v3.20", arch: "x86_64", repos: "main,community", output: "apk2pkg.json", tier: "all", ledger: join(ROOT, "scripts/registry-state.json") };
const argv = process.argv.slice(2);
for (let i = 0; i < argv.length; i++) {
  const next = () => argv[++i];
  if (argv[i] === "--branch") args.branch = next();
  else if (argv[i] === "--arch") args.arch = next();
  else if (argv[i] === "--repos") args.repos = next();
  else if (argv[i] === "-o" || argv[i] === "--output") args.output = next();
  else if (argv[i] === "--tier") args.tier = next();
  else if (argv[i] === "--ledger") args.ledger = next();
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

// Provided by the alpine base package — never useful as a declared dep
// (keep in sync with apk2pkg's BASE_PROVIDES).
const BASE_PROVIDES = new Set([
  "musl", "musl-utils", "busybox", "busybox-binsh", "alpine-baselayout",
  "alpine-baselayout-data", "alpine-keys", "alpine-release", "libc-utils",
]);
// A package someone would DECLARE ships a command; pure libraries reach
// apps vendored inside their dependents' kegs, not by name.
const hasCommand = (p) => p.provides.some((x) => x.startsWith("cmd:"));

// Desktop GUI applications don't belong in a container registry, and they
// are the closure-vendoring worst case (every KDE app ships ~270 MiB of
// Qt+frameworks). Linking a widget toolkit is the tell — X11/xcb alone is
// NOT (ffmpeg and imagemagick link X but are headless workhorses).
const GUI_TOOLKIT = /^so:lib(Qt[56]?|KF[56]|Kirigami|gtk[-_]?[34]?|gdk|adwaita|wx_|SDL2?)/i;
const isGuiApp = (p) => p.depends.some((d) => GUI_TOOLKIT.test(d));

// --- license hygiene: only redistribute what clearly grants redistribution ---
// Alpine's main/community admit only open-source licenses, but the field is
// free text: "custom", "SSH-OpenSSH", "GD" are real permissive licenses
// without an SPDX id. We exclude only what is genuinely unclear — an empty
// field, "Unknown"/"none", or an explicit non-free marker — and show the
// declared string plus the aports link (where the license file lives) for
// everything else.
const licenseProblem = (p) => {
  const expr = (p.license ?? "").trim();
  if (!expr) return "no license declared";
  const parts = expr.split(/\s+(?:AND|OR|WITH)\s+|[()]/i).map((t) => t.trim()).filter(Boolean);
  if (parts.some((t) => /^(unknown|none|proprietary|nonfree|non-free|commercial)$/i.test(t))) return `license "${expr}"`;
  return null;
};

// --- trademark-restricted names: not worth carrying in a CLI registry ---------
// Branded builds whose trademark policies restrict redistribution of the
// official name/artwork. Matched on the apk name.
const TRADEMARKED = /^(firefox|thunderbird|seamonkey|librewolf|tor-browser|waterfox|mozilla-|chrome-|google-chrome|oracle-|java-oracle|mongodb|redis-stack|elasticsearch|kibana|vscode|code-oss|docker-desktop)/i;
const isTrademarked = (p) => TRADEMARKED.test(p.apk);

// --- size cap: the registry serves CLI tools, not toolchains or game data ------
// The target is "about 100 MB per image". Before converting, a package is
// rejected when its apk closure (everything apk2pkg vendors, base excluded)
// downloads more than MAX_CLOSURE_DOWNLOAD — that is roughly the image size
// (squashfs/zstd vs gzip'd apks: typically within ±20%). Packages already
// converted are judged by their real image size against MAX_IMAGE_BYTES,
// the hard ceiling registry-push also enforces after conversion. The gap
// between the two is the accepted rounding error.
const MAX_CLOSURE_DOWNLOAD = 100 * 1000 * 1000; // 100 MB of apks
const MAX_IMAGE_BYTES = 120 * 2 ** 20;           // keep in sync with registry-push.mjs / registry-prune.mjs
const byApk = new Map(packages.map((p) => [p.apk, p]));
const closureDownload = (root) => {
  const seen = new Set();
  const stack = [root.apk];
  let bytes = 0;
  while (stack.length) {
    const apk = stack.pop();
    if (seen.has(apk) || BASE_PROVIDES.has(apk)) continue;
    seen.add(apk);
    const q = byApk.get(apk);
    if (!q) continue;
    bytes += q.size;
    for (const d of q.depends) {
      if (d.startsWith("!")) continue;
      const provider = providerOf[d.split(/[<>=~]/)[0]];
      if (provider) stack.push(provider);
    }
  }
  return bytes;
};
// ply name -> real image size for this arch. An uploaded image is the truth;
// a skipped (too large, never uploaded) record only counts when nothing of
// that name was ever uploaded — a newer, smaller conversion may have made it.
const knownImageBytes = {};
if (existsSync(args.ledger)) {
  const uploaded = {}, skippedOnly = {};
  for (const e of Object.values(JSON.parse(readFileSync(args.ledger, "utf8")))) {
    const arch = e.img?.endsWith("-arm64.img") ? "arm64" : "x64";
    if (arch !== plyArch || !e.bytes) continue;
    const bucket = e.upload_path ? uploaded : skippedOnly;
    bucket[e.name] = Math.max(bucket[e.name] ?? 0, e.bytes);
  }
  for (const [name, bytes] of Object.entries(skippedOnly)) if (!(name in uploaded)) knownImageBytes[name] = bytes;
  Object.assign(knownImageBytes, uploaded);
}
const MiB = (b) => `${Math.round(b / 2 ** 20)} MiB`;
const MB = (b) => `${Math.round(b / 1e6)} MB`;
const tooLarge = (p) => {
  const known = knownImageBytes[p.name];
  if (known !== undefined) return known > MAX_IMAGE_BYTES ? `image ${MiB(known)} > ${MiB(MAX_IMAGE_BYTES)} cap` : null;
  const download = closureDownload(p);
  return download > MAX_CLOSURE_DOWNLOAD ? `closure downloads ${MB(download)} > ${MB(MAX_CLOSURE_DOWNLOAD)} cap` : null;
};

// tier filter (after reverse_deps so counts reflect the whole universe)
let selected = packages;
if (args.tier === "core") selected = packages.filter((p) => !NOISE.test(p.apk));
else if (args.tier === "main-core")
  selected = packages.filter((p) => !NOISE.test(p.apk) && p.repo === "main");
else if (args.tier === "cli")
  // main+community, noise removed, commands only, base internals and
  // desktop GUI apps excluded: the "anything a user would type in
  // [dependencies] on a server" tier
  selected = packages.filter((p) => {
    if (!(!NOISE.test(p.apk) && hasCommand(p) && !BASE_PROVIDES.has(p.apk) && !isGuiApp(p))) return false;
    const why = licenseProblem(p) ?? (isTrademarked(p) ? "trademark-restricted name" : null) ?? tooLarge(p);
    if (why) { skipped.push({ apk: p.apk, apk_version: p.apk_version, repo: p.repo, reason: why }); return false; }
    return true;
  });
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
const count = (re) => skipped.filter((x) => re.test(x.reason)).length;
console.error(`${args.output}: ${selected.length} packages (tier: ${args.tier}), ${skipped.length} skipped (${count(/cap/)} over the size cap, ${count(/license/)} license, ${count(/trademark/)} trademark)`);
