import { registryState, archOf, depLine, fmtSize } from "@/lib/registry";
import { RegistryTable, type Row } from "@/components/RegistryTable";

export const metadata = {
  title: "packages",
  description:
    "The official ply package registry — content-addressed container images, append-only, served from a global CDN.",
};

export default async function RegistryPage() {
  const state = await registryState();

  const rows: Row[] = state.packages.map((p) => {
    const latest = p.versions[p.versions.length - 1];
    const range = latest.version.split(".").slice(0, 2).join(".");
    const byVer = new Map<string, { arch: string; path: string }[]>();
    for (const v of p.versions) {
      if (!byVer.has(v.version)) byVer.set(v.version, []);
      byVer.get(v.version)!.push({ arch: archOf(v), path: v.path });
    }
    return {
      name: p.name,
      namespace: p.namespace,
      description: p.description,
      license: p.license,
      size: fmtSize(latest.bytes),
      dep: depLine(p, range),
      arches: [...new Set(p.versions.map(archOf))].sort().reverse(),
      versions: [...byVer.entries()].map(([version, files]) => ({
        version,
        files: files.sort((a, b) => b.arch.localeCompare(a.arch)),
      })),
    };
  });

  const stats =
    `${state.package_count} packages · ${state.image_count} images · ` +
    `${fmtSize(state.total_bytes)} · updated ${state.updated.slice(0, 16).replace("T", " ")} UTC`;

  return (
    <main className="pt-8 pb-20">
      <h1 className="text-3xl tracking-tight">packages</h1>
      <p className="mt-3 text-sm text-fade">{stats}</p>
      <div className="mt-6 border border-edge bg-card px-4 py-3 max-w-xl">
        <pre className="overflow-x-auto text-sm leading-6"><code>
          <span className="text-fade">[sources]</span>{"\n"}
          default = <span className="text-accent">&quot;https://registry.plybox.sh/ply/&#123;package&#125;&quot;</span>
        </code></pre>
      </div>
      <RegistryTable rows={rows} />
    </main>
  );
}
