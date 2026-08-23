import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { JsonLd } from "@/components/JsonLd";
import { allDocs, docBySlug, sidebar } from "@/lib/docs";
import { absoluteUrl, pageMetadata, SITE_URL } from "@/lib/site";

function requestedDoc(slug?: string[]) {
  if (!slug?.length) return docBySlug("index");
  if (slug.length !== 1 || slug[0] === "index") return undefined;
  return docBySlug(slug[0]);
}

export function generateStaticParams() {
  return allDocs().map((d) => ({
    slug: d.slug === "index" ? [] : [d.slug],
  }));
}

export async function generateMetadata({
  params,
}: {
  params: Promise<{ slug?: string[] }>;
}): Promise<Metadata> {
  const { slug } = await params;
  const doc = requestedDoc(slug);
  return doc
    ? pageMetadata({
        title: doc.title,
        description: doc.description,
        path: doc.url,
        type: "article",
        alternateTypes: { "text/markdown": doc.markdownUrl },
      })
    : {};
}

export default async function DocPage({
  params,
}: {
  params: Promise<{ slug?: string[] }>;
}) {
  const { slug } = await params;
  const doc = requestedDoc(slug);
  if (!doc) notFound();
  const sections = sidebar();

  return (
    <div className="mx-auto w-full max-w-6xl px-5 pb-20 pt-6 sm:px-7 md:flex md:gap-12 md:pt-10">
      <JsonLd
        data={{
          "@context": "https://schema.org",
          "@graph": [
            {
              "@type": "TechArticle",
              "@id": `${absoluteUrl(doc.url)}#article`,
              headline: doc.title,
              description: doc.description,
              url: absoluteUrl(doc.url),
              dateModified: doc.updatedAt,
              inLanguage: "en",
              isPartOf: { "@id": `${SITE_URL}/#website` },
              mainEntityOfPage: absoluteUrl(doc.url),
            },
            {
              "@type": "BreadcrumbList",
              itemListElement: [
                {
                  "@type": "ListItem",
                  position: 1,
                  name: "ply",
                  item: `${SITE_URL}/`,
                },
                {
                  "@type": "ListItem",
                  position: 2,
                  name: "Documentation",
                  item: `${SITE_URL}/docs/`,
                },
                ...(doc.slug === "index"
                  ? []
                  : [
                      {
                        "@type": "ListItem",
                        position: 3,
                        name: doc.title,
                        item: absoluteUrl(doc.url),
                      },
                    ]),
              ],
            },
          ],
        }}
      />
      <details className="utility-surface group mb-8 border border-edge md:hidden">
        <summary className="flex min-h-12 cursor-pointer list-none items-center justify-between px-4 font-mono text-xs text-fade [&::-webkit-details-marker]:hidden">
          <span>
            <span className="mr-2 text-accent">docs /</span>
            {doc.title}
          </span>
          <span aria-hidden="true" className="transition-transform group-open:rotate-45">+</span>
        </summary>
        <nav aria-label="Documentation" className="max-h-[65vh] overflow-y-auto border-t border-edge p-4">
          {sections.map(({ section, pages }) => (
            <div key={section} className="mb-5 last:mb-0">
              <p className="mb-2 font-mono text-[10px] uppercase tracking-wider text-fade">{section}</p>
              {pages.map((page) => (
                <Link
                  key={page.slug}
                  href={page.url}
                  aria-current={page.slug === doc.slug ? "page" : undefined}
                  className={`block min-h-9 py-2 text-sm ${
                    page.slug === doc.slug ? "doc-nav-current" : "text-fade"
                  }`}
                >
                  {page.title}
                </Link>
              ))}
            </div>
          ))}
        </nav>
      </details>

      <aside className="sticky top-20 hidden max-h-[82vh] w-52 shrink-0 self-start overflow-y-auto md:block">
        <nav aria-label="Documentation">
        {sections.map(({ section, pages }) => (
          <div key={section} className="mb-6">
            <div className="mb-2 font-mono text-[10px] uppercase tracking-wider text-fade">
              {section}
            </div>
            {pages.map((p) => (
              <Link
                key={p.slug}
                href={p.url}
                aria-current={p.slug === doc.slug ? "page" : undefined}
                className={`block py-1.5 text-sm transition-colors ${
                  p.slug === doc.slug
                    ? "doc-nav-current"
                    : "text-fade hover:text-ink"
                }`}
              >
                {p.title}
              </Link>
            ))}
          </div>
        ))}
        </nav>
      </aside>
      <article
        className="doc min-w-0 max-w-[72ch] flex-1"
        dangerouslySetInnerHTML={{ __html: doc.html }}
      />
    </div>
  );
}
