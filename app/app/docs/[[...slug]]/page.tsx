import Link from "next/link";
import { notFound } from "next/navigation";
import { allDocs, docBySlug, sidebar } from "@/lib/docs";

export function generateStaticParams() {
  return allDocs().map((d) => ({
    slug: d.slug === "index" ? [] : [d.slug],
  }));
}

export async function generateMetadata({
  params,
}: {
  params: Promise<{ slug?: string[] }>;
}) {
  const { slug } = await params;
  const doc = docBySlug(slug?.[0] ?? "index");
  return doc ? { title: doc.title, description: doc.description } : {};
}

export default async function DocPage({
  params,
}: {
  params: Promise<{ slug?: string[] }>;
}) {
  const { slug } = await params;
  const doc = docBySlug(slug?.[0] ?? "index");
  if (!doc) notFound();

  return (
    <div className="flex gap-10 pt-8 pb-20">
      <aside className="hidden md:block w-52 shrink-0 sticky top-16 self-start max-h-[85vh] overflow-y-auto">
        {sidebar().map(({ section, pages }) => (
          <div key={section} className="mb-6">
            <div className="text-[10px] uppercase tracking-wider text-fade mb-2">
              {section}
            </div>
            {pages.map((p) => (
              <Link
                key={p.slug}
                href={p.url}
                className={`block py-1 text-sm ${
                  p.slug === doc.slug
                    ? "text-accent"
                    : "text-fade hover:text-ink"
                }`}
              >
                {p.title}
              </Link>
            ))}
          </div>
        ))}
      </aside>
      <article
        className="doc min-w-0 max-w-3xl"
        dangerouslySetInnerHTML={{ __html: doc.html }}
      />
    </div>
  );
}
