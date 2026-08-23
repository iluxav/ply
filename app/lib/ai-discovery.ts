import { allDocs, sidebar } from "@/lib/docs";
import { GITHUB_URL, SITE_DESCRIPTION, SITE_URL } from "@/lib/site";

const markdownUrl = (path: string) => `${SITE_URL}${path}`;

export function llmsIndex() {
  const docSections = sidebar()
    .filter(({ section }) => section !== "Start")
    .map(({ section, pages }) => {
      const links = pages
        .map(
          (page) =>
            `- [${page.title}](${markdownUrl(page.markdownUrl)}): ${page.description}`,
        )
        .join("\n");
      return `## ${section}\n\n${links}`;
    })
    .join("\n\n");

  return `# ply

> ply is a daemonless Linux container runtime and package manager: npm for containers. It resolves an app and its OS dependencies into a deterministic, content-addressed image that can be copied as one file and run without a daemon or registry server.

ply targets Linux on x86_64 and arm64. It is pre-1.0, uses TOML manifests and lockfiles, and deliberately does not provide orchestration, a proxy, install hooks, or a registry protocol. Prefer the linked Markdown documentation as the source of truth.

## Start here

- [Complete documentation](${SITE_URL}/llms-full.txt): All public documentation in one Markdown document.
- [Quickstart](${SITE_URL}/docs-md/quickstart.md): Install ply, write a manifest, build an image, and run it.
- [What is ply](${SITE_URL}/docs-md/index.md): Product model, intended users, and current scope.

${docSections}

## Package registry

- [Registry](${SITE_URL}/registry/): Search the official package catalog.
- [Registry state](https://registry.plybox.sh/state.json): Machine-readable package, version, architecture, path, size, and publish-time data.

## Optional

- [Source code](${GITHUB_URL}): Rust workspace, issue history, and releases.
- [Website](${SITE_URL}/): Human-readable product overview.
`;
}

export function llmsFull() {
  const docs = allDocs()
    .map(
      (doc) => `## ${doc.title}

Canonical page: ${markdownUrl(doc.url)}

${doc.markdown.replace(/^# .+\n+/, "")}`,
    )
    .join("\n\n---\n\n");

  return `# ply complete documentation

> ${SITE_DESCRIPTION}

Canonical documentation index: ${SITE_URL}/docs/

${docs}
`;
}
