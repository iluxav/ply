import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  output: "standalone",   // one self-contained server dir → packs into a ply image
  trailingSlash: true,    // preserve the site's existing /docs/<slug>/ URLs
  cacheComponents: true,  // enables `use cache` (registry state caching)
};

export default nextConfig;
