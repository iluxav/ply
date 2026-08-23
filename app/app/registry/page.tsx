import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { Suspense } from "react";
import {
  registryState,
  archOf,
  fmtSize,
  type RegistryPackage,
  type RegistryState,
} from "@/lib/registry";
import {
  RegistryTable,
  type RegistryRow,
} from "@/components/RegistryTable";
import { RegistryPagination } from "@/components/RegistryPagination";
import { CopyButton } from "@/components/CopyButton";
import { JsonLd } from "@/components/JsonLd";
import { pageMetadata, SITE_URL } from "@/lib/site";

const PAGE_SIZE = 100;
const DEFAULT_SOURCE = 'default = "https://registry.plybox.sh/ply/{package}"';

type RegistrySearchParams = {
  page?: string | string[];
  q?: string | string[];
};

type RegistryPageProps = {
  searchParams: Promise<RegistrySearchParams>;
};

function first(value: string | string[] | undefined) {
  return Array.isArray(value) ? value[0] : value;
}

function requestedPage(params: RegistrySearchParams) {
  const value = Number.parseInt(first(params.page) ?? "1", 10);
  return Number.isFinite(value) && value > 0 ? value : 1;
}

function searchQuery(params: RegistrySearchParams) {
  return (first(params.q) ?? "").trim().slice(0, 120);
}

function registryUrl(page: number, query = "") {
  const params = new URLSearchParams();
  if (query) params.set("q", query);
  if (page > 1) params.set("page", String(page));
  const suffix = params.toString();
  return suffix ? `/registry/?${suffix}` : "/registry/";
}

function matchesSearch(pkg: RegistryPackage, query: string) {
  if (!query) return true;
  const searchable = [
    pkg.namespace,
    pkg.name,
    pkg.description,
    pkg.license,
    ...pkg.versions.map((version) => `${version.version} ${archOf(version)}`),
  ]
    .join(" ")
    .toLowerCase();
  return searchable.includes(query.toLowerCase());
}

function packageRow(pkg: RegistryPackage): RegistryRow {
  const latest = pkg.versions[pkg.versions.length - 1];
  const architectures = [
    ...new Set(
      pkg.versions
        .filter((version) => version.version === latest.version)
        .map(archOf),
    ),
  ].sort((a, b) => b.localeCompare(a));

  return {
    name: pkg.name,
    namespace: pkg.namespace,
    description: pkg.description,
    license: pkg.license,
    version: latest.version,
    architectures,
    size: fmtSize(latest.bytes),
  };
}

export async function generateMetadata({
  searchParams,
}: RegistryPageProps): Promise<Metadata> {
  const params = await searchParams;
  const page = requestedPage(params);
  const query = searchQuery(params);
  const title = query
    ? `package search: ${query}`
    : page > 1
      ? `packages — page ${page}`
      : "packages";

  return pageMetadata({
    title,
    description:
      "The official ply package registry — content-addressed container images served as ordinary files.",
    path: query ? "/registry/" : registryUrl(page),
    noIndex: Boolean(query),
    alternateTypes: {
      "application/json": "https://registry.plybox.sh/state.json",
    },
  });
}

async function RegistryCatalog({
  state,
  searchParams,
}: {
  state: RegistryState;
  searchParams: Promise<RegistrySearchParams>;
}) {
  const params = await searchParams;
  const page = requestedPage(params);
  const query = searchQuery(params);
  const packages = state.packages.filter(
    (pkg) => pkg.versions.length > 0 && matchesSearch(pkg, query),
  );
  const pageCount = Math.max(1, Math.ceil(packages.length / PAGE_SIZE));

  if (page > pageCount) notFound();

  const start = (page - 1) * PAGE_SIZE;
  const end = Math.min(start + PAGE_SIZE, packages.length);
  const rows = packages.slice(start, end).map(packageRow);

  return (
    <section className="mt-12" aria-labelledby="package-index-title">
      <div className="flex flex-wrap items-end justify-between gap-5">
        <form action="/registry/" method="get" role="search" className="w-full max-w-xl">
          <label htmlFor="package-search" className="mb-2 block font-mono text-[10px] uppercase tracking-wider text-fade">
            Search every package
          </label>
          <div className="utility-surface flex border border-edge focus-within:border-accent">
            <input
              id="package-search"
              name="q"
              type="search"
              defaultValue={query}
              placeholder="name, description, version, or license"
              autoComplete="off"
              className="min-h-11 min-w-0 flex-1 bg-transparent px-4 font-mono text-sm text-ink placeholder:text-fade focus:outline-none"
            />
            <button
              type="submit"
              className="joined-control min-h-11 border-l border-edge px-4 font-mono text-xs text-fade transition-colors hover:text-accent"
            >
              search
            </button>
          </div>
        </form>

        <div className="flex items-center gap-4 pb-1 font-mono text-[11px] text-fade">
          <p id="package-index-title">
            {packages.length === 0
              ? "0 packages"
              : `showing ${start + 1}–${end} of ${packages.length}`}
          </p>
          {query && (
            <Link href="/registry/" className="text-accent hover:underline">
              clear search
            </Link>
          )}
        </div>
      </div>

      {rows.length > 0 ? (
        <>
          <RegistryTable rows={rows} />
          <RegistryPagination page={page} pageCount={pageCount} query={query} />
        </>
      ) : (
        <div className="mt-6 border-y border-edge py-12 text-center">
          <p className="text-sm text-fade">No packages match “{query}”.</p>
          <Link href="/registry/" className="mt-3 inline-flex min-h-11 items-center font-mono text-xs text-accent hover:underline">
            clear search
          </Link>
        </div>
      )}
    </section>
  );
}

function CatalogFallback() {
  return (
    <div className="mt-12 border-y border-edge py-12 font-mono text-xs text-fade">
      loading package index…
    </div>
  );
}

export default async function RegistryPage({ searchParams }: RegistryPageProps) {
  const state = await registryState();
  const stats =
    `${state.package_count} packages · ${state.image_count} images · ` +
    `${fmtSize(state.total_bytes)} · updated ${state.updated.slice(0, 16).replace("T", " ")} UTC`;

  return (
    <main className="mx-auto w-full max-w-[1480px] px-5 pb-20 pt-10 sm:px-7 sm:pt-14">
      <JsonLd
        data={{
          "@context": "https://schema.org",
          "@type": "CollectionPage",
          "@id": `${SITE_URL}/registry/#catalog`,
          name: "ply package registry",
          description:
            "The official catalog of content-addressed packages for ply.",
          url: `${SITE_URL}/registry/`,
          inLanguage: "en",
          isPartOf: { "@id": `${SITE_URL}/#website` },
          mainEntity: {
            "@type": "ItemList",
            name: "ply packages",
            numberOfItems: state.package_count,
          },
        }}
      />
      <div>
        <div className="max-w-3xl">
          <p className="eyebrow">official registry</p>
          <h1 className="mt-3 text-4xl font-medium tracking-[-0.04em] sm:text-5xl">
            Package registry.
          </h1>
          <p className="mt-4 max-w-2xl text-base leading-7 text-fade">
            Content-addressed images served as ordinary files. Search the catalog,
            lock a digest, and fetch it from any mirror.
          </p>
          <p className="mt-4 font-mono text-[11px] text-fade">{stats}</p>
        </div>

        <div className="utility-surface mt-7 flex max-w-3xl items-center border border-edge">
          <span className="hidden shrink-0 border-r border-edge px-3 font-mono text-[10px] uppercase tracking-wider text-fade sm:inline-flex sm:min-h-11 sm:items-center">
            source
          </span>
          <code className="min-w-0 flex-1 overflow-x-auto whitespace-nowrap px-4 font-mono text-xs text-accent">
            {DEFAULT_SOURCE}
          </code>
          <CopyButton
            value={`[sources]\n${DEFAULT_SOURCE}`}
            label="copy registry source"
            iconOnly
            className="joined-control shrink-0 border-y-0 border-r-0"
          />
        </div>
      </div>

      <Suspense fallback={<CatalogFallback />}>
        <RegistryCatalog state={state} searchParams={searchParams} />
      </Suspense>
    </main>
  );
}
