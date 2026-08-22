import type { Metadata } from "next";
import Link from "next/link";
import "./globals.css";

export const metadata: Metadata = {
  metadataBase: new URL("https://plybox.sh"),
  title: { default: "ply — npm for containers", template: "%s · ply" },
  description:
    "A daemonless container runtime and package manager. One static binary, deterministic images, any file host is a registry.",
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <body className="bg-ground text-ink font-mono antialiased">
        <div className="mx-auto w-full max-w-6xl px-5">
          <header className="sticky top-0 z-10 bg-[#0b100df0] flex items-baseline justify-between py-4 border-b border-edge/50">
            <div className="flex items-center gap-1">
              <Link href="/" className="text-lg tracking-tight logo">ply</Link>
              <span className="text-sm tracking-tight text-accent opacity-70">box</span>
            </div>
            <nav className="flex gap-6 text-sm text-fade">
              <Link href="/docs/" className="hover:text-accent">docs</Link>
              <Link href="/registry/" className="hover:text-accent">registry</Link>
              <a href="https://github.com/iluxav/ply" className="hover:text-accent">github</a>
            </nav>
          </header>
          {children}
          <footer className="border-t border-edge py-8 mt-16 text-xs text-fade flex flex-wrap gap-x-6 gap-y-2">
            <span>content-addressed · append-only · any file host is a mirror</span>
            <a href="https://registry.plybox.sh" className="hover:text-accent">registry.plybox.sh</a>
          </footer>
        </div>
      </body>
    </html>
  );
}
