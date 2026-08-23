import Link from "next/link";

export type RegistryRow = {
  name: string;
  namespace: string;
  description: string;
  license: string;
  version: string;
  architectures: string[];
  size: string;
};

function PackageIdentity({ row }: { row: RegistryRow }) {
  return (
    <div className="min-w-0">
      <Link
        href={`/registry/${row.name}/`}
        className="font-mono text-sm text-ink transition-colors hover:text-accent"
      >
        {row.namespace !== "ply" && (
          <span className="text-fade">{row.namespace}/</span>
        )}
        {row.name}
      </Link>
      <p className="mt-1 line-clamp-2 max-w-2xl text-sm leading-5 text-fade">
        {row.description || "No package description."}
      </p>
    </div>
  );
}

function Architectures({ values }: { values: string[] }) {
  return (
    <span className="font-mono text-xs text-fade">
      {values.length > 0 ? values.join(" · ") : "—"}
    </span>
  );
}

function MobileRow({ row }: { row: RegistryRow }) {
  return (
    <article className="py-5 first:pt-0 last:pb-0">
      <PackageIdentity row={row} />
      <dl className="mt-4 grid grid-cols-2 gap-x-5 gap-y-3 font-mono text-[11px] sm:grid-cols-4">
        <div>
          <dt className="text-fade">latest</dt>
          <dd className="mt-1 text-ink">{row.version}</dd>
        </div>
        <div>
          <dt className="text-fade">architectures</dt>
          <dd className="mt-1"><Architectures values={row.architectures} /></dd>
        </div>
        <div>
          <dt className="text-fade">size</dt>
          <dd className="mt-1 text-ink">{row.size}</dd>
        </div>
        <div className="min-w-0">
          <dt className="text-fade">license</dt>
          <dd className="mt-1 truncate text-ink">{row.license || "—"}</dd>
        </div>
      </dl>
    </article>
  );
}

export function RegistryTable({ rows }: { rows: RegistryRow[] }) {
  if (rows.length === 0) return null;

  return (
    <>
      <div className="ply-panel mt-6 divide-y divide-edge border border-edge px-4 lg:hidden">
        {rows.map((row) => (
          <MobileRow key={`${row.namespace}/${row.name}`} row={row} />
        ))}
      </div>

      <div className="registry-table-frame mt-6 hidden border border-edge lg:block">
        <table className="w-full table-fixed text-left">
          <colgroup>
            <col className="w-[48%]" />
            <col className="w-[12%]" />
            <col className="w-[14%]" />
            <col className="w-[10%]" />
            <col className="w-[16%]" />
          </colgroup>
          <thead className="registry-table-head sticky top-[61px] z-10">
            <tr className="border-b border-edge font-mono text-[10px] uppercase tracking-wider text-fade">
              <th className="px-5 py-3 font-normal">package</th>
              <th className="px-5 py-3 font-normal">latest</th>
              <th className="px-5 py-3 font-normal">architectures</th>
              <th className="px-5 py-3 text-right font-normal">size</th>
              <th className="px-5 py-3 font-normal">license</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => (
              <tr
                key={`${row.namespace}/${row.name}`}
                className="border-b border-edge align-top transition-colors last:border-b-0 hover:bg-card"
              >
                <td className="px-5 py-4"><PackageIdentity row={row} /></td>
                <td className="px-5 py-4 font-mono text-xs text-ink">{row.version}</td>
                <td className="px-5 py-4"><Architectures values={row.architectures} /></td>
                <td className="whitespace-nowrap px-5 py-4 text-right font-mono text-xs text-fade">{row.size}</td>
                <td className="truncate px-5 py-4 font-mono text-xs text-fade" title={row.license || undefined}>
                  {row.license || "—"}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}
