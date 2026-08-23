import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import {
  registryState,
  archOf,
  alpineLinks,
  depLine,
  fmtSize,
  type RegistryPackage,
} from "@/lib/registry";
import { CopyButton } from "@/components/CopyButton";
import { JsonLd } from "@/components/JsonLd";
import { absoluteUrl, pageMetadata, SITE_URL } from "@/lib/site";

function packageSummary(pkg: RegistryPackage) {
  if (pkg.description) return pkg.description;
  const latest = pkg.versions.at(-1);
  if (!latest) return `${pkg.name} in the official ply package registry.`;
  const architectures = [
    ...new Set(
      pkg.versions
        .filter((version) => version.version === latest.version)
        .map(archOf),
    ),
  ];
  return `${pkg.name} ${latest.version} for Linux ${architectures.join(" and ")}. Browse available builds and add the package to ply.toml.`;
}

export async function generateMetadata({
  params,
}: {
  params: Promise<{ name: string }>;
}): Promise<Metadata> {
  const { name } = await params;
  const p = (await registryState()).packages.find((x) => x.name === name);
  return p
    ? pageMetadata({
        title: `${p.name} package`,
        description: packageSummary(p),
        path: `/registry/${encodeURIComponent(p.name)}/`,
      })
    : {};
}

export default async function PackagePage({
  params,
}: {
  params: Promise<{ name: string }>;
}) {
  const { name } = await params;
  const state = await registryState();
  const p = state.packages.find((x) => x.name === name);
  if (!p) notFound();

  const latest = p.versions[p.versions.length - 1];
  const range = latest.version.split(".").slice(0, 2).join(".");
  const dependency = depLine(p, range);
  const description = packageSummary(p);
  const packageUrl = absoluteUrl(`/registry/${encodeURIComponent(p.name)}/`);
  const latestBuilds = p.versions.filter(
    (version) => version.version === latest.version,
  );
  const architectures = [...new Set(latestBuilds.map(archOf))];

  return (
    <main className="mx-auto w-full max-w-3xl px-5 pb-20 pt-10 sm:px-7 sm:pt-14">
      <JsonLd
        data={{
          "@context": "https://schema.org",
          "@graph": [
            {
              "@type": "SoftwareApplication",
              "@id": `${packageUrl}#package`,
              name: p.name,
              description,
              url: packageUrl,
              sameAs: p.homepage || undefined,
              applicationCategory: "DeveloperApplication",
              operatingSystem: "Linux",
              softwareVersion: latest.version,
              softwareRequirements: `Linux on ${architectures.join(" or ")}`,
              downloadUrl: latestBuilds.map(
                (version) => `https://registry.plybox.sh/${version.path}`,
              ),
              license: p.license || undefined,
              isPartOf: { "@id": `${SITE_URL}/registry/#catalog` },
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
                  name: "Registry",
                  item: `${SITE_URL}/registry/`,
                },
                {
                  "@type": "ListItem",
                  position: 3,
                  name: p.name,
                  item: packageUrl,
                },
              ],
            },
          ],
        }}
      />
      <p className="font-mono text-xs text-fade">
        <Link href="/registry/" className="hover:text-accent">packages</Link> / {p.namespace}
      </p>
      <h1 className="mt-4 text-4xl font-medium tracking-[-0.04em] sm:text-5xl">{p.name}</h1>
      <p className="mt-4 max-w-2xl text-lg leading-8 text-fade">{description}</p>
      <p className="mt-4 font-mono text-xs text-fade">
        {p.license && <span className="mr-4">license: {p.license}</span>}
        {p.homepage && (
          <a href={p.homepage} className="text-accent hover:underline">{p.homepage}</a>
        )}
      </p>
      {p.alpine && (
        <p className="mt-2 font-mono text-xs text-fade">
          unmodified Alpine Linux {p.alpine.branch} build ·{" "}
          <a href={alpineLinks(p.alpine).package} className="text-accent hover:underline">
            package
          </a>{" "}
          ·{" "}
          <a href={alpineLinks(p.alpine).source} className="text-accent hover:underline">
            source (aports)
          </a>
        </p>
      )}

      <h2 className="mt-10 font-mono text-[10px] uppercase tracking-wider text-fade">use it</h2>
      <div className="utility-surface mt-2 flex items-stretch border border-edge">
        <pre className="min-w-0 flex-1 overflow-x-auto px-4 py-3 font-mono text-sm leading-6"><code>
          <span className="text-fade">[dependencies]</span>{"\n"}
          <span className="text-accent">{dependency}</span>
        </code></pre>
        <CopyButton value={`[dependencies]\n${dependency}`} className="joined-control shrink-0 border-y-0 border-r-0" />
      </div>

      <h2 className="mt-10 font-mono text-[10px] uppercase tracking-wider text-fade">versions</h2>
      <div className="mt-2 overflow-x-auto border border-edge">
      <table className="w-full min-w-lg text-sm">
        <thead className="sr-only">
          <tr><th>Version</th><th>Architecture</th><th>Size</th><th>Published</th></tr>
        </thead>
        <tbody>
          {[...p.versions].reverse().map((v) => (
            <tr key={v.img} className="border-b border-edge last:border-b-0">
              <td className="whitespace-nowrap px-4 py-3 font-mono">{v.version}</td>
              <td className="px-4 py-2">
                <a
                  href={`https://registry.plybox.sh/${v.path}`}
                  className="secondary-action inline-flex min-h-8 items-center border border-edge px-2 font-mono text-[10px] text-fade transition-colors hover:border-accent hover:text-accent"
                >
                  {archOf(v)}
                </a>
              </td>
              <td className="whitespace-nowrap px-4 py-3 font-mono text-xs text-fade">{fmtSize(v.bytes)}</td>
              <td className="whitespace-nowrap px-4 py-3 font-mono text-xs text-fade">
                {v.pushed_at?.slice(0, 10)}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      </div>
    </main>
  );
}
