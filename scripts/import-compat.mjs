#!/usr/bin/env node
// Import-compatibility harness: how much of Docker Hub actually runs on ply?
//
//   ./scripts/import-compat.mjs                          # the whole default set
//   ./scripts/import-compat.mjs --tier service           # only privilege-dropping services
//   ./scripts/import-compat.mjs --only redis,nginx       # specific entries
//   ./scripts/import-compat.mjs --list                   # print the set and exit
//   ./scripts/import-compat.mjs --resume                 # skip entries already in results.json
//   ./scripts/import-compat.mjs --keep                   # keep the .img files (default: delete)
//   ./scripts/import-compat.mjs --run-timeout 20         # seconds a service must survive to pass
//   ./scripts/import-compat.mjs --privileged             # skip rights stripping (triage: is the
//                                                        # capability drop what is blocking it?)
//
// Two questions per image, in order:
//   1. does `ply import docker://…` produce an image at all?
//   2. does `ply run` on that image get the entrypoint to stay up?
//
// (2) is the interesting one. ply drops the whole capability bounding set,
// so an entrypoint doing `chown -R x:x /data && exec gosu x …` — which is
// what nearly every official service image does — loses CAP_CHOWN and
// CAP_SETUID/SETGID that Docker grants by default. seccomp is a blocklist
// returning EPERM (looser than Docker's), so it is the less likely culprit.
// The classifier below tells the two apart from the captured output.
//
// Runs ROOTLESS by default — no sudo, the same path a new user takes. ply
// chooses its mode from its own euid, so to test the rootful path run the
// whole script under sudo. The modes fail differently and it matters:
// rootless enters a user namespace with exactly ONE uid mapped, so any
// chown/setuid to a service uid gets EINVAL; rootful has no userns at all.
//
// Big images cost real disk (mysql/mongo are ~600-800 MiB flattened). Use
// --tier / --only to bound a run; images are deleted after each entry
// unless --keep.

import { spawn } from "node:child_process";
import {
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  writeFileSync,
  rmSync,
  statSync,
} from "node:fs";

// unique capture-file suffix per run within this process
let captureSeq = 0;
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));

// --- the image set -----------------------------------------------------------
// kind decides the pass condition:
//   service — must still be alive at --run-timeout (it is a daemon)
//   oneshot — must exit 0 (it is a tool or a bare shell)
// env is what the image needs to not refuse on purpose (postgres et al).
const IMAGES = [
  // Services that drop privileges in their entrypoint — the hard cases.
  { name: "redis",     ref: "redis:7-alpine",        kind: "service", tier: "service" },
  { name: "postgres",  ref: "postgres:16-alpine",    kind: "service", tier: "service", env: { POSTGRES_PASSWORD: "test" } },
  { name: "nginx",     ref: "nginx:1.27-alpine",     kind: "service", tier: "service" },
  { name: "mariadb",   ref: "mariadb:11",            kind: "service", tier: "service", env: { MARIADB_ROOT_PASSWORD: "test" } },
  { name: "mysql",     ref: "mysql:8",               kind: "service", tier: "service", env: { MYSQL_ROOT_PASSWORD: "test" } },
  { name: "mongo",     ref: "mongo:7",               kind: "service", tier: "service" },
  { name: "rabbitmq",  ref: "rabbitmq:3-alpine",     kind: "service", tier: "service" },
  { name: "memcached", ref: "memcached:1.6-alpine",  kind: "service", tier: "service" },

  // Proxies / web servers.
  { name: "caddy",     ref: "caddy:2-alpine",        kind: "service", tier: "proxy" },
  { name: "traefik",   ref: "traefik:v3",            kind: "service", tier: "proxy" },
  { name: "httpd",     ref: "httpd:2.4-alpine",      kind: "service", tier: "proxy" },
  // haproxy ships NO default config: `docker run haproxy` fails the same way
  // ("Cannot open configuration file /usr/local/etc/haproxy/haproxy.cfg"),
  // so it cannot be a compatibility signal until the harness can mount one.
  // { name: "haproxy", ref: "haproxy:lts-alpine", kind: "service", tier: "proxy" },

  // Language runtimes — usually stay root, should be the easy tier.
  { name: "node",      ref: "node:22-alpine",        kind: "oneshot", tier: "runtime", argvNote: "default CMD is node REPL" },
  { name: "python",    ref: "python:3.12-alpine",    kind: "oneshot", tier: "runtime" },
  { name: "golang",    ref: "golang:1.23-alpine",    kind: "oneshot", tier: "runtime" },
  { name: "ruby",      ref: "ruby:3.3-alpine",       kind: "oneshot", tier: "runtime" },
  { name: "php",       ref: "php:8.3-cli-alpine",    kind: "oneshot", tier: "runtime" },
  { name: "temurin",   ref: "eclipse-temurin:21-jre", kind: "oneshot", tier: "runtime" },

  // Baselines — if these fail, something is wrong with the harness, not ply.
  { name: "alpine",    ref: "alpine:3.20",           kind: "oneshot", tier: "base" },
  { name: "busybox",   ref: "busybox:1.36",          kind: "oneshot", tier: "base" },
  { name: "debian",    ref: "debian:bookworm-slim",  kind: "oneshot", tier: "base" },
  { name: "hello",     ref: "hello-world:latest",    kind: "oneshot", tier: "base" },
];

// --- failure classification --------------------------------------------------
// Ordered: first match wins, most specific first. `cause` is what to fix,
// `detail` is the evidence line pulled from the output. Import and run
// signatures are kept apart — a run failure must never be blamed on the
// registry (an early version matched bare "denied" and called redis's
// `find: Permission denied` an auth failure).
const IMPORT_SIGNATURES = [
  { cause: "import:arch",          re: /no linux\/(amd64|arm64) manifest/i },
  { cause: "import:no-entrypoint", re: /has no Entrypoint\/Cmd/i },
  { cause: "import:no-layers",     re: /manifest has no layers/i },
  { cause: "import:auth",          re: /(HTTP (401|403)|\bunauthorized\b|authentication required|pull rate limit)/i },
  { cause: "import:network",       re: /(dns|timed out|connection refused|tls handshake|certificate)/i },
  { cause: "import:blob",          re: /(blob|layer|manifest)[^\n]*(failed|error|unexpected)/i },
];

const RUN_SIGNATURES = [
  // --- ply is starting the app in the wrong directory ---
  // The image declared a WORKDIR that ply neither imports (oci.rs OciConfig
  // has no WorkingDir) nor honours (run.rs hardcodes /opt/<app>, and
  // container.rs silently falls back to / when that chdir fails). Entrypoints
  // that walk `.` then rampage over the whole rootfs — the tell is errors on
  // ./proc or ./sys, which no sane WORKDIR contains.
  { cause: "ply:wrong-cwd",        re: /(chown|chmod|find|rm|cp)[^\n]*\.\/(proc|sys)\//i },

  // --- rootless user namespace maps exactly one uid ---
  // run.rs writes `uid_map: 0 <uid> 1`, so uid 0 is the only id that exists
  // inside. chown/setuid to any other id (redis is 999, postgres 70) has no
  // mapping and the kernel answers EINVAL — not EPERM, which is what makes
  // this distinguishable from a missing capability. The real fix is a
  // /etc/subuid range applied via newuidmap/newgidmap, as rootless
  // podman/docker do. Rootful ply never enters a userns, so it is unaffected.
  { cause: "userns:unmapped-uid",  re: /(chown|chgrp)[^\n]*invalid argument/i },
  { cause: "userns:unmapped-uid",  re: /(setuid|setgid|setgroups|initgroups)[^\n]*invalid argument/i },
  { cause: "userns:unmapped-uid",  re: /\b(gosu|su-exec)\b[^\n]*invalid argument/i },

  // --- privileged port under rootless: a Linux limit, not a ply bug ---
  // Rootless shares the HOST netns, and CAP_NET_BIND_SERVICE inside a user
  // namespace does not authorize binding <1024 in the host's netns. Rootless
  // docker/podman have the identical limitation. Must be matched BEFORE the
  // capability and log signatures: httpd reports "Unable to open logs" as its
  // LAST line after failing to bind, and traefik's message says "opening
  // listener", both of which look like something else entirely.
  { cause: "rootless:privileged-port", re: /(bind|listen)[^\n]*:[0-9]{1,3}\b[^\n]*permission denied/i },
  { cause: "rootless:privileged-port", re: /permission denied[^\n]*(bind|listen)[^\n]*:[0-9]{1,3}\b/i },
  { cause: "rootless:privileged-port", re: /error opening listener[^\n]*listen/i },

  // --- the image declares USER, which ply import drops on the floor ---
  // oci.rs OciConfig has no `User` field, so the app runs as root. Some
  // daemons refuse that outright (memcached exits 64 with this exact line).
  { cause: "oci:user-ignored",     re: /must add '-u root' to start as root/i },
  { cause: "oci:user-ignored",     re: /(refus|will not|do not|don't) run[^\n]*as root/i },

  // --- log files symlinked to /dev/stdout|stderr will not open ---
  // Kept as a tripwire: this fired for nginx and httpd until the harness
  // stopped capturing through node's stdio:"pipe" (a socketpair — and a
  // socket cannot be reopened via /proc/self/fd/N, so open() returns ENXIO).
  // Capture is a real file now. If this ever fires again it means something
  // OTHER than the harness handed the container an unopenable stderr.
  { cause: "ply:dev-stdio",        re: /open\(\)[^\n]*\.log[^\n]*failed[^\n]*No such device or address/i },
  { cause: "ply:dev-stdio",        re: /Unable to open logs/i },

  // --- capabilities ---
  { cause: "cap:CAP_CHOWN",        re: /chown[^\n]*(operation not permitted|permission denied)/i },
  { cause: "cap:CAP_CHOWN",        re: /(operation not permitted)[^\n]*chown/i },
  { cause: "cap:CAP_SETUID",       re: /\b(gosu|su-exec|setpriv|runuser|su):/i },
  { cause: "cap:CAP_SETUID",       re: /(setuid|setgid|setgroups|initgroups)[^\n]*(not permitted|denied)/i },
  { cause: "cap:CAP_SETUID",       re: /unable to (setgid|setuid|drop privileges)/i },
  { cause: "cap:CAP_DAC_OVERRIDE", re: /(mkdir|open|create|write)[^\n]*permission denied/i },
  { cause: "cap:CAP_KILL",         re: /kill[^\n]*operation not permitted/i },
  { cause: "cap:CAP_NET_RAW",      re: /(raw socket|icmp)[^\n]*(not permitted|denied)/i },
  { cause: "cap:CAP_NET_BIND",     re: /bind[^\n]*(permission denied|operation not permitted)/i },

  // --- seccomp blocklist (EPERM on a listed syscall) ---
  { cause: "seccomp:mount",        re: /\bmount\b[^\n]*(operation not permitted|eperm)/i },
  { cause: "seccomp:unshare",      re: /\bunshare\b[^\n]*(not permitted|eperm)/i },
  { cause: "seccomp:ptrace",       re: /\bptrace\b[^\n]*(not permitted|eperm)/i },
  { cause: "seccomp:keyctl",       re: /\b(keyctl|add_key|request_key)\b[^\n]*(not permitted|eperm)/i },
  { cause: "seccomp:bpf",          re: /\bbpf\b[^\n]*(not permitted|eperm)/i },
  { cause: "seccomp:perf_event",   re: /perf_event_open[^\n]*(not permitted|eperm)/i },

  // --- ply runtime plumbing ---
  { cause: "ply:exec-not-found",   re: /not on the image's PATH/i },
  { cause: "ply:setup",            re: /container setup failed/i },
  { cause: "ply:rights",           re: /rights stripping failed/i },
  { cause: "ply:userns",           re: /unprivileged user namespaces|sudo ply setup/i },
  { cause: "ply:no-devpts",        re: /no \/dev\/pts in rootless/i },

  // --- generic userland fallbacks ---
  { cause: "app:missing-lib",      re: /(error loading shared libraries|no such file or directory: .*\.so)/i },
  { cause: "app:readonly-fs",      re: /read-only file system/i },
  { cause: "app:tty",              re: /(no tty|not a terminal|inappropriate ioctl)/i },
  { cause: "app:generic-eperm",    re: /operation not permitted/i },
  { cause: "app:generic-eacces",   re: /permission denied/i },
];

function classify(text, phase) {
  const table = phase === "import" ? IMPORT_SIGNATURES : RUN_SIGNATURES;
  for (const sig of table) {
    const m = sig.re.exec(text);
    if (m) {
      const line =
        text
          .split("\n")
          .find((l) => sig.re.test(l))
          ?.trim()
          .slice(0, 200) ?? m[0];
      return { cause: sig.cause, detail: line };
    }
  }
  return { cause: "unknown", detail: lastMeaningfulLine(text) };
}

function lastMeaningfulLine(text) {
  const lines = text
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
  return lines.length ? lines[lines.length - 1].slice(0, 200) : "(no output)";
}

// --- args --------------------------------------------------------------------
const args = {
  ply: "ply",
  outDir: join(ROOT, "out/import-compat"),
  tier: null,
  only: null,
  importTimeout: 600,
  runTimeout: 15,
  rootful: false,
  keep: false,
  resume: false,
  list: false,
  privileged: false,
};

const argv = process.argv.slice(2);
for (let i = 0; i < argv.length; i++) {
  const a = argv[i];
  const next = () => argv[++i];
  if (a === "--tier") args.tier = next();
  else if (a === "--only") args.only = next().split(",").map((s) => s.trim());
  else if (a === "--out") args.outDir = next();
  else if (a === "--ply") args.ply = next();
  else if (a === "--import-timeout") args.importTimeout = Number(next());
  else if (a === "--run-timeout") args.runTimeout = Number(next());
  else if (a === "--rootful") args.rootful = true;
  else if (a === "--privileged") args.privileged = true;
  else if (a === "--keep") args.keep = true;
  else if (a === "--resume") args.resume = true;
  else if (a === "--list") args.list = true;
  else if (a === "-h" || a === "--help") {
    console.log(readFileSync(new URL(import.meta.url)).toString().split("\n").slice(1, 27).join("\n"));
    process.exit(0);
  } else {
    console.error(`unknown flag: ${a}`);
    process.exit(2);
  }
}

let selected = IMAGES;
if (args.tier) selected = selected.filter((i) => i.tier === args.tier);
if (args.only) selected = selected.filter((i) => args.only.includes(i.name));

if (args.list) {
  for (const i of selected) console.log(`${i.name.padEnd(11)} ${i.ref.padEnd(28)} ${i.kind}  [${i.tier}]`);
  console.log(`\n${selected.length} image(s)`);
  process.exit(0);
}
if (!selected.length) {
  console.error("no images selected");
  process.exit(2);
}

// --- process helpers ---------------------------------------------------------
// Everything ply prints that matters goes to stderr; merge both so the
// classifier sees one stream in order.
//
// Capture goes through a real FILE, never node's "pipe". Node's stdio:"pipe"
// is a socketpair, and a socket CANNOT be reopened through /proc/self/fd/N —
// open() returns ENXIO. Images that write logs via /dev/stdout or /dev/stderr
// (nginx, httpd symlink their logs there) then die at startup with
// `open() "/var/log/nginx/error.log" failed (6: No such device or address)`,
// which looks exactly like a runtime bug and is not one: docker hands
// containers a pipe or a tty, never a socket. A regular file reopens fine.
function runCapture(cmd, cmdArgs, { timeoutSec, env, killAfterTimeout }) {
  return new Promise((resolve) => {
    const started = Date.now();
    const capturePath = join(
      args.outDir,
      `.capture-${process.pid}-${captureSeq++}.log`,
    );
    const capFd = openSync(capturePath, "w+");
    const child = spawn(cmd, cmdArgs, {
      env: { ...process.env, ...(env ?? {}) },
      stdio: ["ignore", capFd, capFd],
    });
    const readCapture = () => {
      try {
        const text = readFileSync(capturePath, "utf8");
        return text.length > 256 * 1024 ? text.slice(-256 * 1024) : text;
      } catch {
        return "";
      }
    };
    const cleanup = () => {
      try { closeSync(capFd); } catch { /* already closed */ }
      try { rmSync(capturePath, { force: true }); } catch { /* best effort */ }
    };

    let timedOut = false;
    let hardKilled = false;
    const timer = setTimeout(() => {
      timedOut = true;
      // SIGTERM lets ply's InstanceGuard unmount layers and clean the
      // instance dir. SIGKILL would leak mounts, so it is last resort.
      child.kill("SIGTERM");
      setTimeout(() => {
        if (child.exitCode === null && child.signalCode === null) {
          hardKilled = true;
          child.kill("SIGKILL");
        }
      }, 5000);
    }, timeoutSec * 1000);

    child.on("error", (e) => {
      clearTimeout(timer);
      const out = readCapture();
      cleanup();
      resolve({ spawnError: String(e), out, code: null, signal: null, timedOut: false, hardKilled, ms: Date.now() - started });
    });
    child.on("close", (code, signal) => {
      clearTimeout(timer);
      const out = readCapture();
      cleanup();
      resolve({ spawnError: null, out, code, signal, timedOut, hardKilled, ms: Date.now() - started });
    });

    void killAfterTimeout;
  });
}

// ply picks rootless vs rootful from its own euid, so the harness never
// prefixes sudo — run the whole script under sudo to test the rootful path.
// The two modes fail differently (rootless has a one-uid userns), so the
// report says which one produced it.
const amRoot = typeof process.getuid === "function" && process.getuid() === 0;
if (args.rootful && !amRoot) {
  console.error("--rootful needs the script itself run as root: sudo ./scripts/import-compat.mjs …");
  process.exit(2);
}
const rootful = args.rootful || amRoot;
const plyCmd = [args.ply];

// --- results -----------------------------------------------------------------
mkdirSync(args.outDir, { recursive: true });
const resultsPath = join(args.outDir, "results.json");
let results = {};
if (args.resume && existsSync(resultsPath)) {
  results = JSON.parse(readFileSync(resultsPath, "utf8"));
  console.log(`resuming — ${Object.keys(results).length} entry/entries already recorded`);
}
const saveResults = () => writeFileSync(resultsPath, JSON.stringify(results, null, 1));

// --- the loop ----------------------------------------------------------------
console.log(
  `ply import compatibility — ${selected.length} image(s), ${rootful ? "ROOTFUL" : "rootless"}, run-timeout ${args.runTimeout}s${args.privileged ? ", PRIVILEGED" : ""}\n`,
);

for (const [n, image] of selected.entries()) {
  if (args.resume && results[image.name]) {
    console.log(`[${n + 1}/${selected.length}] ${image.name} — skipped (already recorded)`);
    continue;
  }

  const prefix = `[${n + 1}/${selected.length}] ${image.name.padEnd(10)}`;
  const imgPath = join(args.outDir, `${image.name}.img`);
  const record = { name: image.name, ref: image.ref, kind: image.kind, tier: image.tier, rootful, privileged: args.privileged };

  // ---- step 1: import ----
  process.stdout.write(`${prefix} import… `);
  const imp = await runCapture(plyCmd[0], [...plyCmd.slice(1), "import", `docker://${image.ref}`, "-o", imgPath], {
    timeoutSec: args.importTimeout,
  });
  record.import = {
    ok: imp.code === 0 && existsSync(imgPath),
    code: imp.code,
    seconds: Math.round(imp.ms / 100) / 10,
    timedOut: imp.timedOut,
  };
  if (imp.spawnError) {
    console.log(`SPAWN ERROR — ${imp.spawnError}`);
    record.status = "harness-error";
    record.cause = "harness:spawn";
    record.detail = imp.spawnError;
    results[image.name] = record;
    saveResults();
    continue;
  }
  if (!record.import.ok) {
    const { cause, detail } = classify(imp.out, "import");
    record.status = "import-failed";
    record.cause = cause;
    record.detail = detail;
    record.output = imp.out.slice(-4000);
    console.log(`FAILED (${cause})`);
    results[image.name] = record;
    saveResults();
    continue;
  }
  record.import.bytes = statSync(imgPath).size;
  const mib = (record.import.bytes / 1048576).toFixed(1);
  process.stdout.write(`ok ${mib} MiB in ${record.import.seconds}s · run… `);

  // ---- step 2: run ----
  const runArgs = [...plyCmd.slice(1), "run", imgPath];
  if (args.privileged) runArgs.push("--privileged");
  for (const [k, v] of Object.entries(image.env ?? {})) runArgs.push("-e", `${k}=${v}`);
  const run = await runCapture(plyCmd[0], runArgs, { timeoutSec: args.runTimeout });

  record.run = {
    code: run.code,
    signal: run.signal,
    seconds: Math.round(run.ms / 100) / 10,
    survivedTimeout: run.timedOut,
    hardKilled: run.hardKilled,
  };
  record.output = run.out.slice(-4000);

  // Pass conditions differ by kind. A service must still be alive when we
  // stop it; a oneshot must have exited cleanly on its own.
  let pass;
  if (image.kind === "service") {
    pass = run.timedOut;
  } else {
    pass = !run.timedOut && run.code === 0;
  }

  if (pass) {
    record.status = "pass";
    record.cause = null;
    console.log(`PASS`);
  } else {
    const { cause, detail } = classify(run.out, "run");
    record.status = "run-failed";
    record.cause = cause;
    record.detail = detail;
    // exit codes ply assigns itself carry more signal than the text
    if (run.code === 127) record.cause = record.cause === "unknown" ? "ply:exec-not-found" : record.cause;
    if (run.code === 126) record.cause = "ply:rights";
    if (run.code === 125) record.cause = "ply:parent-died";
    console.log(`FAIL (${record.cause}) exit=${run.code ?? run.signal}`);
    if (record.detail && record.detail !== "(no output)") console.log(`${" ".repeat(prefix.length)}  ↳ ${record.detail}`);
  }

  if (run.hardKilled) console.log(`${" ".repeat(prefix.length)}  ⚠ SIGKILLed — check \`ply ps\` and /run/ply for leaked mounts`);

  results[image.name] = record;
  saveResults();
  if (!args.keep) rmSync(imgPath, { force: true });
}

// --- report ------------------------------------------------------------------
const rows = Object.values(results);
const pass = rows.filter((r) => r.status === "pass");
const byCause = {};
for (const r of rows.filter((r) => r.status !== "pass")) {
  (byCause[r.cause] ??= []).push(r.name);
}

const pct = rows.length ? Math.round((pass.length / rows.length) * 100) : 0;
const lines = [];
lines.push(`# ply import compatibility`);
lines.push("");
lines.push(`**${pass.length}/${rows.length} images ran (${pct}%)** — ${rootful ? "rootful" : "rootless"}, run-timeout ${args.runTimeout}s${args.privileged ? ", **--privileged**" : ""}`);
lines.push("");
lines.push(`| image | ref | kind | status | size | cause | evidence |`);
lines.push(`|---|---|---|---|---|---|---|`);
for (const r of rows) {
  const size = r.import?.bytes ? `${(r.import.bytes / 1048576).toFixed(1)} MiB` : "—";
  const mark = r.status === "pass" ? "✅ pass" : r.status === "import-failed" ? "⛔ import" : "❌ run";
  const evidence = (r.detail ?? "").replace(/\|/g, "\\|");
  lines.push(`| ${r.name} | \`${r.ref}\` | ${r.kind} | ${mark} | ${size} | ${r.cause ?? ""} | ${evidence} |`);
}
lines.push("");
lines.push(`## Failures by cause`);
lines.push("");
if (!Object.keys(byCause).length) {
  lines.push("None.");
} else {
  lines.push(`| cause | count | images |`);
  lines.push(`|---|---|---|`);
  for (const [cause, names] of Object.entries(byCause).sort((a, b) => b[1].length - a[1].length)) {
    lines.push(`| \`${cause}\` | ${names.length} | ${names.join(", ")} |`);
  }
}
lines.push("");

const mdPath = join(args.outDir, "report.md");
writeFileSync(mdPath, lines.join("\n"));

console.log("\n" + lines.join("\n"));
console.log(`\nresults: ${resultsPath}`);
console.log(`report:  ${mdPath}`);
