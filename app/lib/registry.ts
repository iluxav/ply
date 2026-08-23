// The registry's machine-readable snapshot, cached server-side: visitors
// never download the raw state; the droplet refetches on a short lifetime.
import { cacheLife } from "next/cache";

export type RegistryVersion = {
  version: string;
  img: string;
  arch?: "x64" | "arm64";
  path: string;
  bytes: number;
  pushed_at: string;
};

export type AlpineProvenance = {
  branch: string; // e.g. "v3.20"
  repo: string; // "main" | "community"
  apk: string; // Alpine package name
  origin: string; // source package (aports directory)
};

export type RegistryPackage = {
  namespace: string;
  name: string;
  description: string;
  license: string;
  homepage: string;
  alpine?: AlpineProvenance;
  versions: RegistryVersion[];
};

// Where an unmodified Alpine build came from: the package page and the
// aports recipe (APKBUILD, license file, source tarball reference).
export const alpineLinks = (a: AlpineProvenance) => ({
  package: `https://pkgs.alpinelinux.org/package/${a.branch}/${a.repo}/x86_64/${a.apk}`,
  source: `https://git.alpinelinux.org/aports/tree/${a.repo}/${a.origin}?h=${a.branch.replace(/^v/, "")}-stable`,
});

export type RegistryState = {
  updated: string;
  package_count: number;
  image_count: number;
  total_bytes: number;
  packages: RegistryPackage[];
};

export const archOf = (v: RegistryVersion): "x64" | "arm64" =>
  v.arch ?? (v.img.endsWith("-arm64.img") ? "arm64" : "x64");

export async function registryState(): Promise<RegistryState> {
  "use cache";
  cacheLife("minutes");
  const res = await fetch("https://registry.plybox.sh/state.json", {
    headers: { "User-Agent": "plybox-web" },
  });
  if (!res.ok) throw new Error(`state.json: HTTP ${res.status}`);
  return res.json();
}

// TOML: a bare key containing a dot is a nested table — quote such names
export const depLine = (p: RegistryPackage, range: string) => {
  const key = p.name.includes(".") ? `"${p.name}"` : p.name;
  return p.namespace === "ply"
    ? `${key} = "${range}"`
    : `${key} = { source = "${p.namespace}", version = "${range}" }`;
};

export const fmtSize = (b: number) =>
  !b ? "—"
  : b >= 1 << 30 ? (b / (1 << 30)).toFixed(2) + " GiB"
  : b >= 1 << 20 ? (b / (1 << 20)).toFixed(1) + " MiB"
  : Math.round(b / 1024) + " KiB";
