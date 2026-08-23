// Docs come from the repo's own docs/*.md and are synced into .content before
// dev/build. Public routes render at build time; keeping the small source copy
// in the standalone output also lets unknown dynamic routes return a clean 404.
import fs from "node:fs";
import path from "node:path";
import matter from "gray-matter";
import { Marked } from "marked";
import hljs from "highlight.js";

const DOCS_DIR = path.join(process.cwd(), ".content", "docs");
const SECTIONS = ["Start", "Guides", "Reference", "Concepts"];

export type DocPage = {
  slug: string;          // "index" | "quickstart" | …
  url: string;           // "/docs/" | "/docs/quickstart/"
  markdownUrl: string;
  title: string;
  description: string;
  section: string;
  order: number;
  markdown: string;
  updatedAt: string;
  html: string;
};

const slugify = (t: string) =>
  t.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");

const marked = new Marked({
  renderer: {
    heading({ text, depth }) {
      const raw = text.replace(/<[^>]*>/g, "");
      const id = slugify(raw);
      const anchor =
        depth === 2 || depth === 3
          ? `<a class="anchor" href="#${id}" aria-label="link to section">#</a>`
          : "";
      return `<h${depth} id="${id}">${anchor}${text}</h${depth}>\n`;
    },
    code({ text, lang }) {
      const language = lang && hljs.getLanguage(lang) ? lang : undefined;
      const html = language
        ? hljs.highlight(text, { language }).value
        : hljs.highlightAuto(text).value;
      return `<pre><code class="hljs">${html}</code></pre>\n`;
    },
  },
});

let cache: DocPage[] | null = null;

export function allDocs(): DocPage[] {
  if (cache) return cache;
  const pages: DocPage[] = [];
  for (const file of fs.readdirSync(DOCS_DIR)) {
    if (!file.endsWith(".md")) continue;
    const { data, content } = matter(
      fs.readFileSync(path.join(DOCS_DIR, file), "utf8"),
    );
    if (!data.title) continue; // design notes (e.g. ply-vm.md) aren't site pages
    const slug = path.basename(file, ".md");
    const updatedAt = fs.statSync(path.join(DOCS_DIR, file)).mtime.toISOString();
    pages.push({
      slug,
      url: slug === "index" ? "/docs/" : `/docs/${slug}/`,
      markdownUrl: `/docs-md/${slug}.md`,
      title: data.title,
      description: data.description ?? "",
      section: data.section ?? "Guides",
      order: data.order ?? 99,
      markdown: content.trim(),
      updatedAt,
      html: marked.parse(content) as string,
    });
  }
  pages.sort(
    (a, b) =>
      SECTIONS.indexOf(a.section) - SECTIONS.indexOf(b.section) ||
      a.order - b.order,
  );
  cache = pages;
  return pages;
}

export function docBySlug(slug: string): DocPage | undefined {
  return allDocs().find((d) => d.slug === slug);
}

export function sidebar(): { section: string; pages: DocPage[] }[] {
  const bySection = new Map<string, DocPage[]>();
  for (const d of allDocs()) {
    if (!bySection.has(d.section)) bySection.set(d.section, []);
    bySection.get(d.section)!.push(d);
  }
  return [...bySection.entries()].map(([section, pages]) => ({ section, pages }));
}
