import Link from "next/link";

type RegistryPaginationProps = {
  page: number;
  pageCount: number;
  query: string;
};

function registryHref(page: number, query: string) {
  const params = new URLSearchParams();
  if (query) params.set("q", query);
  if (page > 1) params.set("page", String(page));
  const suffix = params.toString();
  return suffix ? `/registry/?${suffix}` : "/registry/";
}

export function RegistryPagination({
  page,
  pageCount,
  query,
}: RegistryPaginationProps) {
  if (pageCount <= 1) return null;

  return (
    <nav
      aria-label="Package pages"
      className="mt-8 flex items-center justify-between gap-4 border-t border-edge pt-5 font-mono text-xs"
    >
      {page > 1 ? (
        <Link
          href={registryHref(page - 1, query)}
          rel="prev"
          className="inline-flex min-h-11 items-center text-fade transition-colors hover:text-accent"
        >
          ← previous
        </Link>
      ) : (
        <span aria-hidden="true" />
      )}

      <span className="text-fade">
        page <span className="text-ink">{page}</span> of {pageCount}
      </span>

      {page < pageCount ? (
        <Link
          href={registryHref(page + 1, query)}
          rel="next"
          className="inline-flex min-h-11 items-center text-fade transition-colors hover:text-accent"
        >
          next →
        </Link>
      ) : (
        <span aria-hidden="true" />
      )}
    </nav>
  );
}
