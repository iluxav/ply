import Link from "next/link";
import { notFound } from "next/navigation";
import { registryState, archOf, depLine, fmtSize } from "@/lib/registry";

export async function generateMetadata({
  params,
}: {
  params: Promise<{ name: string }>;
}) {
  const { name } = await params;
  const p = (await registryState()).packages.find((x) => x.name === name);
  return p
    ? {
        title: `${p.name} — ply package`,
        description: p.description || `${p.name} in the ply registry`,
      }
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

  return (
    <main className="pt-8 pb-20 max-w-3xl">
      <p className="text-xs text-fade">
        <Link href="/registry/" className="hover:text-accent">packages</Link> / {p.namespace}
      </p>
      <h1 className="mt-2 text-3xl tracking-tight">{p.name}</h1>
      {p.description && <p className="mt-3 text-fade">{p.description}</p>}
      <p className="mt-2 text-xs text-fade">
        {p.license && <span className="mr-4">license: {p.license}</span>}
        {p.homepage && (
          <a href={p.homepage} className="text-accent hover:underline">{p.homepage}</a>
        )}
      </p>

      <h2 className="mt-8 text-sm uppercase tracking-wider text-fade">use it</h2>
      <div className="mt-2 border border-edge bg-card px-4 py-3">
        <pre className="overflow-x-auto text-sm leading-6"><code>
          <span className="text-fade">[dependencies]</span>{"\n"}
          {depLine(p, range)}
        </code></pre>
      </div>

      <h2 className="mt-8 text-sm uppercase tracking-wider text-fade">versions</h2>
      <table className="mt-2 w-full text-sm border border-edge">
        <tbody>
          {[...p.versions].reverse().map((v) => (
            <tr key={v.img} className="border-b border-edge last:border-b-0">
              <td className="px-4 py-2 whitespace-nowrap">{v.version}</td>
              <td className="px-4 py-2">
                <a
                  href={`https://registry.plybox.sh/${v.path}`}
                  className="border border-edge px-1 py-px text-[10px] text-fade hover:text-accent hover:border-accent"
                >
                  {archOf(v)}
                </a>
              </td>
              <td className="px-4 py-2 text-fade whitespace-nowrap">{fmtSize(v.bytes)}</td>
              <td className="px-4 py-2 text-fade text-xs whitespace-nowrap">
                {v.pushed_at?.slice(0, 10)}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </main>
  );
}
