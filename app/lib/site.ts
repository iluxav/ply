import type { Metadata } from "next";

export const SITE_NAME = "ply";
export const SITE_URL = "https://plybox.sh";
export const GITHUB_URL = "https://github.com/iluxav/ply";
export const SITE_DESCRIPTION =
  "npm for containers: a daemonless Linux container runtime and package manager. Build a deterministic image, move one file, run it — no registry server.";

export function absoluteUrl(path = "/") {
  return new URL(path, `${SITE_URL}/`).toString();
}

type PageMetadataOptions = {
  title: string;
  description: string;
  path: string;
  type?: "website" | "article";
  noIndex?: boolean;
  alternateTypes?: Record<string, string>;
};

export function pageMetadata({
  title,
  description,
  path,
  type = "website",
  noIndex = false,
  alternateTypes,
}: PageMetadataOptions): Metadata {
  const url = absoluteUrl(path);

  return {
    title,
    description,
    alternates: {
      canonical: url,
      types: alternateTypes,
    },
    openGraph: {
      type,
      locale: "en_US",
      siteName: SITE_NAME,
      title,
      description,
      url,
    },
    twitter: {
      card: "summary_large_image",
      title,
      description,
    },
    robots: noIndex ? { index: false, follow: true } : undefined,
  };
}
