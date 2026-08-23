import type { Metadata } from "next";
import Link from "next/link";
import { IBM_Plex_Sans } from "next/font/google";
import { JsonLd } from "@/components/JsonLd";
import {
  GITHUB_URL,
  SITE_DESCRIPTION,
  SITE_NAME,
  SITE_URL,
} from "@/lib/site";
import "./globals.css";

const plexSans = IBM_Plex_Sans({
  subsets: ["latin"],
  variable: "--font-plex-sans",
  display: "swap",
});

export const metadata: Metadata = {
  metadataBase: new URL(SITE_URL),
  title: { default: "ply — npm for containers", template: "%s · ply" },
  description: SITE_DESCRIPTION,
  applicationName: SITE_NAME,
  category: "developer tools",
  creator: SITE_NAME,
  publisher: SITE_NAME,
  openGraph: {
    type: "website",
    locale: "en_US",
    siteName: SITE_NAME,
    title: "ply — npm for containers",
    description: SITE_DESCRIPTION,
    url: SITE_URL,
  },
  twitter: {
    card: "summary_large_image",
    title: "ply — npm for containers",
    description: SITE_DESCRIPTION,
  },
  robots: { index: true, follow: true },
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" className={plexSans.variable}>
      <head>
        <link rel="describedby" href="/llms.txt" type="text/markdown" />
      </head>
      <body className="bg-ground font-sans text-ink antialiased">
        <JsonLd
          data={{
            "@context": "https://schema.org",
            "@type": "WebSite",
            "@id": `${SITE_URL}/#website`,
            name: SITE_NAME,
            alternateName: "plybox",
            url: `${SITE_URL}/`,
            description: SITE_DESCRIPTION,
            inLanguage: "en",
          }}
        />
        <header className="sticky top-0 z-20 border-b border-edge/60 bg-ground/95 backdrop-blur-sm">
          <div className="mx-auto flex w-full max-w-[1480px] items-center justify-between px-5 py-2 sm:px-7">
            <div className="flex items-center gap-1">
              <Link href="/" className="logo font-mono text-lg tracking-tight" aria-label="ply home">ply</Link>
              <span className="font-mono text-xs tracking-tight text-fade">box</span>
            </div>
            <nav aria-label="Primary navigation" className="flex items-center gap-1 font-mono text-[13px] text-fade sm:gap-2">
              <Link href="/docs/" className="inline-flex min-h-11 items-center px-2 transition-colors hover:text-accent sm:px-3">docs</Link>
              <Link href="/registry/" className="inline-flex min-h-11 items-center px-2 transition-colors hover:text-accent sm:px-3">registry</Link>
              <a
                target="_blank"
                rel="noreferrer"
                href={GITHUB_URL}
                className="hidden min-h-11 items-center px-3 transition-colors hover:text-accent sm:inline-flex"
              >
                github<span className="sr-only"> (opens in a new tab)</span>
              </a>
            </nav>
          </div>
        </header>
        {children}
      </body>
    </html>
  );
}
