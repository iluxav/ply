#!/usr/bin/env node
// Render docs/*.md → static HTML at web/dist/docs/<slug>/index.html.
// Pure build-time: marked + highlight.js run here, the CDN serves flat files.
// Frontmatter per page: title, description, section, order.

import { readFileSync, writeFileSync, mkdirSync, readdirSync } from "node:fs";
import { dirname, join, basename } from "node:path";
import { fileURLToPath } from "node:url";
import { Marked } from "marked";
import hljs from "highlight.js";

const WEB = dirname(fileURLToPath(import.meta.url));
const ROOT = dirname(WEB);
const SRC = join(ROOT, "docs");
const OUT = join(WEB, "dist/docs");

const SECTIONS = ["Start", "Guides", "Reference", "Concepts"]; // sidebar order

const esc = (s) => String(s)
  .replaceAll("&", "&amp;").replaceAll("<", "&lt;")
  .replaceAll(">", "&gt;").replaceAll('"', "&quot;");

// --- load pages ---------------------------------------------------------------
function frontmatter(raw) {
  const m = raw.match(/^---\n([\s\S]*?)\n---\n/);
  if (!m) return [{}, raw];
  const meta = {};
  for (const line of m[1].split("\n")) {
    const i = line.indexOf(":");
    if (i > 0) meta[line.slice(0, i).trim()] = line.slice(i + 1).trim().replace(/^["']|["']$/g, "");
  }
  return [meta, raw.slice(m[0].length)];
}

const pages = [];
for (const file of readdirSync(SRC)) {
  if (!file.endsWith(".md")) continue;
  const [meta, body] = frontmatter(readFileSync(join(SRC, file), "utf8"));
  if (!meta.title) continue; // only pages that opted in via frontmatter
  const slug = basename(file, ".md");
  pages.push({
    slug,
    url: slug === "index" ? "/docs/" : `/docs/${slug}/`,
    title: meta.title,
    description: meta.description ?? "",
    section: meta.section ?? "Guides",
    order: parseInt(meta.order ?? "99", 10),
    body,
  });
}
pages.sort((a, b) =>
  SECTIONS.indexOf(a.section) - SECTIONS.indexOf(b.section) || a.order - b.order);

// --- markdown pipeline --------------------------------------------------------
const slugify = (t) => t.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");

function highlight(code, lang) {
  if (lang && hljs.getLanguage(lang)) return hljs.highlight(code, { language: lang }).value;
  return esc(code);
}

const marked = new Marked({
  renderer: {
    heading({ tokens, depth }) {
      const text = this.parser.parseInline(tokens);
      const id = slugify(text.replace(/<[^>]*>/g, ""));
      return depth === 1
        ? `<h1>${text}</h1>\n`
        : `<h${depth} id="${id}"><a class="anchor" href="#${id}" aria-label="link to section">#</a>${text}</h${depth}>\n`;
    },
    code({ text, lang }) {
      return `<div class="codeblock"><button class="copy" aria-label="copy">copy</button>` +
        `<pre><code class="hljs">${highlight(text, lang)}</code></pre></div>\n`;
    },
  },
});

// --- template -----------------------------------------------------------------
function sidebar(current) {
  return SECTIONS.map((section) => {
    const items = pages.filter((p) => p.section === section);
    if (!items.length) return "";
    const links = items.map((p) =>
      `<a href="${p.url}" class="block px-3 py-1.5 text-sm ${p.url === current.url
        ? "text-accent border-l-2 border-accent bg-card"
        : "text-fade hover:text-ink border-l-2 border-transparent"}">${esc(p.title)}</a>`).join("\n");
    return `<div class="mb-6"><div class="px-3 mb-2 text-xs uppercase tracking-wider text-fade">${section}</div>${links}</div>`;
  }).join("\n");
}

function render(page, i) {
  const prev = pages[i - 1], next = pages[i + 1];
  const content = marked.parse(page.body);
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(page.title)} — ply docs</title>
<meta name="description" content="${esc(page.description)}">
<link rel="icon" type="image/svg+xml" href="/logo.svg">
<link rel="canonical" href="https://plybox.sh${page.url}">
<link rel="stylesheet" href="/styles.css">
</head>
<body class="bg-ground text-ink font-mono antialiased">

<header class="sticky top-0 z-20 bg-ground/95 backdrop-blur border-b border-edge">
  <div class="mx-auto max-w-7xl px-5 flex items-center justify-between h-14">
    <div class="flex items-center gap-3">
      <button id="menu" class="lg:hidden text-fade border border-edge px-2 py-1 text-sm" aria-label="menu">≡</button>
      <a href="/" class="logo">ply</a>
      <a href="/docs/" class="text-sm text-fade hover:text-accent">docs</a>
    </div>
    <div class="flex items-center gap-5">
      <input id="search" type="search" placeholder="/ search docs…" autocomplete="off"
             class="hidden sm:block w-56 border border-edge bg-card px-3 py-1.5 text-sm placeholder:text-fade focus:border-accent focus:outline-none">
      <nav class="flex gap-5 text-sm text-fade">
        <a href="https://registry.plybox.sh" class="hover:text-accent">registry</a>
        <a href="https://github.com/iluxav/ply" class="hover:text-accent">github</a>
      </nav>
    </div>
  </div>
  <div id="results" class="hidden absolute left-0 right-0 top-14 border-b border-edge bg-card">
    <div class="mx-auto max-w-7xl px-5 py-3" id="results-list"></div>
  </div>
</header>

<div class="mx-auto max-w-7xl px-5 flex">
  <aside id="sidebar" class="hidden lg:block w-60 shrink-0 border-r border-edge py-8 pr-2
                             fixed lg:sticky top-14 bottom-0 lg:h-[calc(100vh-3.5rem)] overflow-y-auto bg-ground z-10">
${sidebar(page)}
  </aside>
  <main class="prose min-w-0 flex-1 py-10 lg:pl-10 max-w-3xl">
${content}
    <div class="mt-16 pt-6 border-t border-edge flex justify-between text-sm">
      ${prev ? `<a class="text-fade hover:text-accent" href="${prev.url}">← ${esc(prev.title)}</a>` : "<span></span>"}
      ${next ? `<a class="text-fade hover:text-accent" href="${next.url}">${esc(next.title)} →</a>` : "<span></span>"}
    </div>
  </main>
</div>

<footer class="border-t border-edge mt-4">
  <div class="mx-auto max-w-7xl px-5 py-8 text-xs text-fade flex flex-wrap gap-x-6 gap-y-2">
    <span>ply — daemonless containers</span>
    <a href="https://plybox.sh" class="hover:text-accent">plybox.sh</a>
    <a href="https://registry.plybox.sh" class="hover:text-accent">registry</a>
  </div>
</footer>

<script src="/docs/docs.js" defer></script>
</body>
</html>`;
}

// --- emit ---------------------------------------------------------------------
let count = 0;
for (const [i, page] of pages.entries()) {
  const dir = page.slug === "index" ? OUT : join(OUT, page.slug);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "index.html"), render(page, i));
  count++;
}

// search index: title + section + plain text (tags stripped, truncated)
const searchIndex = pages.map((p) => ({
  title: p.title,
  section: p.section,
  url: p.url,
  text: p.body.replace(/```[\s\S]*?```/g, " ").replace(/[#*_`\[\]()|]/g, " ")
    .replace(/\s+/g, " ").toLowerCase().slice(0, 4000),
}));
writeFileSync(join(OUT, "search-index.json"), JSON.stringify(searchIndex));

// shared client JS: search + copy buttons + mobile sidebar
writeFileSync(join(OUT, "docs.js"), `
const search = document.getElementById("search");
const results = document.getElementById("results");
const list = document.getElementById("results-list");
let index = null;
async function ensureIndex() {
  if (!index) index = await (await fetch("/docs/search-index.json")).json();
  return index;
}
search?.addEventListener("input", async () => {
  const q = search.value.trim().toLowerCase();
  if (q.length < 2) { results.classList.add("hidden"); return; }
  const idx = await ensureIndex();
  const hits = idx.filter((p) => p.title.toLowerCase().includes(q) || p.text.includes(q)).slice(0, 8);
  list.innerHTML = hits.length
    ? hits.map((h) => \`<a class="block py-1.5 text-sm text-fade hover:text-accent" href="\${h.url}"><span class="text-ink">\${h.title}</span> · \${h.section}</a>\`).join("")
    : '<div class="py-1.5 text-sm text-fade">no matches</div>';
  results.classList.remove("hidden");
});
document.addEventListener("click", (e) => {
  if (!results.contains(e.target) && e.target !== search) results.classList.add("hidden");
});
document.addEventListener("keydown", (e) => {
  if (e.key === "/" && document.activeElement !== search) { e.preventDefault(); search?.focus(); }
  if (e.key === "Escape") results.classList.add("hidden");
});
document.getElementById("menu")?.addEventListener("click", () => {
  document.getElementById("sidebar").classList.toggle("hidden");
});
for (const btn of document.querySelectorAll(".codeblock .copy")) {
  btn.addEventListener("click", () => {
    navigator.clipboard.writeText(btn.nextElementSibling.textContent).then(() => {
      btn.textContent = "✓"; setTimeout(() => { btn.textContent = "copy"; }, 1200);
    });
  });
}
`);

// sitemap for the whole site
const urls = ["https://plybox.sh/", "https://registry.plybox.sh/",
  ...pages.map((p) => `https://plybox.sh${p.url}`)];
writeFileSync(join(WEB, "dist/sitemap.xml"),
  `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n` +
  urls.map((u) => `  <url><loc>${u}</loc></url>`).join("\n") + "\n</urlset>\n");

console.log(`rendered ${count} docs pages + search index + sitemap`);
