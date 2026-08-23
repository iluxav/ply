import { allDocs, docBySlug } from "@/lib/docs";
import { absoluteUrl } from "@/lib/site";

export function generateStaticParams() {
  return allDocs().map((doc) => ({ slug: `${doc.slug}.md` }));
}

export async function GET(
  _request: Request,
  { params }: { params: Promise<{ slug: string }> },
) {
  const { slug } = await params;
  const doc = slug.endsWith(".md") ? docBySlug(slug.slice(0, -3)) : undefined;

  if (!doc) {
    return new Response("Not found\n", {
      status: 404,
      headers: { "Content-Type": "text/plain; charset=utf-8" },
    });
  }

  const markdown = `---
title: ${JSON.stringify(doc.title)}
description: ${JSON.stringify(doc.description)}
canonical: ${absoluteUrl(doc.url)}
---

${doc.markdown}
`;

  return new Response(markdown, {
    headers: {
      "Content-Type": "text/markdown; charset=utf-8",
      "Cache-Control": "public, max-age=3600, s-maxage=86400",
      Link: `<${absoluteUrl(doc.url)}>; rel="canonical", <${absoluteUrl("/llms.txt")}>; rel="describedby"`,
    },
  });
}
