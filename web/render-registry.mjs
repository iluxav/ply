#!/usr/bin/env node
// Pre-render the registry page: bake state.json into registry/index.html so
// the package table is real HTML (SEO / no-JS crawlers), not a client fetch.
//
// Used two ways:
//   - imported by scripts/registry-push.mjs (renders from the fresh state)
//   - CLI: `node web/render-registry.mjs` fetches the live state.json and
//     writes dist/registry-index.html (used by web/push.sh)

import { readFileSync, writeFileSync, mkdirSync, realpathSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const WEB = dirname(fileURLToPath(import.meta.url));

const esc = (s) => String(s)
  .replaceAll("&", "&amp;").replaceAll("<", "&lt;")
  .replaceAll(">", "&gt;").replaceAll('"', "&quot;");

const fmtSize = (b) => !b ? "—"
  : b >= 1 << 30 ? (b / (1 << 30)).toFixed(2) + " GiB"
  : b >= 1 << 20 ? (b / (1 << 20)).toFixed(1) + " MiB"
  : Math.round(b / 1024) + " KiB";

export function renderRegistry(state) {
  const template = readFileSync(join(WEB, "registry/index.html"), "utf8");

  const rows = state.packages.map((p) => {
    const latest = p.versions[p.versions.length - 1];
    const versions = p.versions
      .map((v) => `<a class="text-accent hover:underline" href="/${esc(v.path)}">${esc(v.version)}</a>`)
      .join(", ");
    const label = p.namespace === "ply"
      ? esc(p.name)
      : `<span class="text-fade">${esc(p.namespace)}/</span>${esc(p.name)}`;
    // manifest line: latest version relaxed to major.minor (the range style
    // demos use); non-default namespaces need an explicit source alias
    const range = latest.version.split(".").slice(0, 2).join(".");
    const dep = p.namespace === "ply"
      ? `${p.name} = "${range}"`
      : `${p.name} = { source = "${p.namespace}", version = "${range}" }`;
    const search = esc(`${p.namespace}/${p.name} ${p.description}`.toLowerCase());
    return `<tr class="border-b border-edge last:border-b-0 align-top" data-search="${search}">
      <td class="px-4 py-3 whitespace-nowrap">${label}<button
        class="copy-dep ml-2 border border-edge px-1.5 py-0.5 text-[10px] text-fade hover:text-accent align-middle"
        data-dep="${esc(dep)}" title="copy ply.toml dependency line">copy</button></td>
      <td class="px-4 py-3">${versions}</td>
      <td class="px-4 py-3 whitespace-nowrap text-fade">${fmtSize(latest.bytes)}</td>
      <td class="px-4 py-3 text-fade max-w-48 break-words">${esc(p.license || "—")}</td>
      <td class="px-4 py-3 text-fade">${esc(p.description || "")}</td>
    </tr>`;
  }).join("\n");

  const stats = `${state.package_count} packages · ${state.image_count} images · ` +
    `${fmtSize(state.total_bytes)} · updated ${esc(state.updated.slice(0, 16).replace("T", " "))} UTC`;

  return template.replace("<!--stats-->", stats).replace("<!--rows-->", rows);
}

const isMain = process.argv[1] &&
  realpathSync(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  const res = await fetch("https://registry.plybox.sh/state.json");
  if (!res.ok) { console.error(`state.json: HTTP ${res.status}`); process.exit(1); }
  const html = renderRegistry(await res.json());
  mkdirSync(join(WEB, "dist"), { recursive: true });
  const out = join(WEB, "dist/registry-index.html");
  writeFileSync(out, html);
  console.log(`rendered ${out}`);
}
