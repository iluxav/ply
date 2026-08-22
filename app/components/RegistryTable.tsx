"use client";
import Link from "next/link";
import { useMemo, useState, useEffect, useRef } from "react";

export type Row = {
  name: string;
  namespace: string;
  description: string;
  license: string;
  size: string;
  dep: string;
  arches: string[];
  versions: { version: string; files: { arch: string; path: string }[] }[];
};

function CopyDep({ dep }: { dep: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      className="copy-dep ml-2 border border-edge px-1.5 py-0.5 text-[10px] text-fade hover:text-accent align-middle"
      title="copy ply.toml dependency line"
      onClick={() => {
        navigator.clipboard.writeText(dep).then(() => {
          setCopied(true);
          setTimeout(() => setCopied(false), 1200);
        });
      }}
    >
      {copied ? "✓" : "copy"}
    </button>
  );
}

export function RegistryTable({ rows }: { rows: Row[] }) {
  const [q, setQ] = useState("");
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "/" && document.activeElement !== input.current) {
        e.preventDefault();
        input.current?.focus();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, []);

  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return rows;
    return rows.filter((r) =>
      `${r.namespace}/${r.name} ${r.description} ${r.arches.join(" ")}`
        .toLowerCase()
        .includes(needle),
    );
  }, [q, rows]);

  return (
    <>
      <input
        ref={input}
        type="search"
        placeholder="/ filter packages…"
        autoComplete="off"
        value={q}
        onChange={(e) => setQ(e.target.value)}
        className="mt-6 w-full max-w-md border border-edge bg-card px-4 py-2.5 text-sm placeholder:text-fade focus:border-accent focus:outline-none"
      />
      <div className="mt-6 overflow-x-auto border border-edge">
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b border-edge text-left text-xs uppercase tracking-wider text-fade">
              <th className="px-4 py-3 font-normal">package</th>
              <th className="px-4 py-3 font-normal">versions</th>
              <th className="px-4 py-3 font-normal">size</th>
              <th className="px-4 py-3 font-normal">license</th>
              <th className="px-4 py-3 font-normal">description</th>
            </tr>
          </thead>
          <tbody>
            {shown.map((r) => (
              <tr key={`${r.namespace}/${r.name}`} className="border-b border-edge last:border-b-0 align-top">
                <td className="px-4 py-3 whitespace-nowrap">
                  <Link href={`/registry/${r.name}/`} className="hover:text-accent">
                    {r.namespace !== "ply" && <span className="text-fade">{r.namespace}/</span>}
                    {r.name}
                  </Link>
                  <CopyDep dep={r.dep} />
                </td>
                <td className="px-4 py-3">
                  {r.versions.map((v, i) => (
                    <span key={v.version} className="whitespace-nowrap">
                      {i > 0 && ", "}
                      {v.version}{" "}
                      {v.files.map((f) => (
                        <a
                          key={f.arch}
                          href={`https://registry.plybox.sh/${f.path}`}
                          className="border border-edge px-1 py-px text-[10px] text-fade hover:text-accent hover:border-accent mr-1"
                        >
                          {f.arch}
                        </a>
                      ))}
                    </span>
                  ))}
                </td>
                <td className="px-4 py-3 whitespace-nowrap text-fade">{r.size}</td>
                <td className="px-4 py-3 text-fade max-w-48 break-words">{r.license || "—"}</td>
                <td className="px-4 py-3 text-fade">{r.description}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {shown.length === 0 && (
        <p className="mt-6 text-sm text-fade">no packages match.</p>
      )}
    </>
  );
}
