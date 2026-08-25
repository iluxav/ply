#!/usr/bin/env node
// skills/ply/ is the source of truth. This emits the two forms people
// actually consume, into app/public/, at build time — so there is never a
// checked-in copy to drift from the source.
//
//   node scripts/build-skill.mjs
//     → app/public/ply-skill.md   one file to paste into any agent
//     → app/public/ply.skill      installable package (zip)
//
// Run from `sync-content`, alongside the docs copy.
import { readFileSync, writeFileSync, readdirSync, mkdirSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));
const SRC = join(ROOT, "skills/ply");
const OUT = join(ROOT, "app/public");
mkdirSync(OUT, { recursive: true });

const skill = readFileSync(join(SRC, "SKILL.md"), "utf8");
const lines = skill.split("\n");
const end = lines.indexOf("---", 1);
const description = lines
  .slice(1, end)
  .find((l) => l.startsWith("description:"))
  .slice("description: ".length);

// --- single file: frontmatter becomes a lede, references are appended ---
const refDir = join(SRC, "references");
const refs = readdirSync(refDir).filter((f) => f.endsWith(".md")).sort();
const body = lines
  .slice(end + 1)
  .join("\n")
  .replace(/## Reference files\n[\s\S]*$/, "---\n\nThe sections below are the full references.\n");

const single = [
  "# ply — agent guide",
  "",
  `> ${description}`,
  "",
  body,
  ...refs.map((f) => "\n---\n\n" + readFileSync(join(refDir, f), "utf8")),
].join("\n");
writeFileSync(join(OUT, "ply-skill.md"), single);

// --- installable package: a plain zip rooted at ply/ ---
execFileSync("zip", ["-qr", join(OUT, "ply.skill"), "ply", "-x", "*/evals/*"], {
  cwd: dirname(SRC),
});

console.log(
  `skill: ply-skill.md (${single.split("\n").length} lines) + ply.skill from ${refs.length + 1} source files`,
);
