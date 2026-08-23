import type { MetadataRoute } from "next";
import { allDocs } from "@/lib/docs";
import { registryState } from "@/lib/registry";
import { SITE_URL } from "@/lib/site";

export default async function sitemap(): Promise<MetadataRoute.Sitemap> {
  const state = await registryState();
  const registryUpdated = new Date(state.updated);
  const docs = allDocs();

  return [
    {
      url: `${SITE_URL}/`,
      changeFrequency: "monthly",
      priority: 1,
    },
    ...docs.map((doc) => ({
      url: `${SITE_URL}${doc.url}`,
      lastModified: doc.updatedAt,
      changeFrequency: "monthly" as const,
      priority: doc.slug === "index" ? 0.8 : 0.7,
    })),
    {
      url: `${SITE_URL}/registry/`,
      lastModified: registryUpdated,
      changeFrequency: "daily",
      priority: 0.9,
    },
    ...state.packages.map((pkg) => ({
      url: `${SITE_URL}/registry/${encodeURIComponent(pkg.name)}/`,
      lastModified: pkg.versions.at(-1)?.pushed_at || registryUpdated,
      changeFrequency: "weekly" as const,
      priority: 0.7,
    })),
  ];
}
