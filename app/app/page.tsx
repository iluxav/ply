import type { Metadata } from "next";
import Link from "next/link";
import { CopyButton } from "@/components/CopyButton";
import { HeroSlides } from "@/components/HeroSlides";
import { JsonLd } from "@/components/JsonLd";
import { GITHUB_URL, SITE_DESCRIPTION, SITE_URL } from "@/lib/site";

export const metadata: Metadata = {
  alternates: { canonical: "/" },
};

const INSTALL_COMMAND = "curl -fsSL https://plybox.sh/install.sh | sh";

const PRINCIPLES = [
  {
    token: "ply.toml",
    title: "Packages, not layers",
    body: "Dependencies are named, declared, and resolved once. No anonymous build-cache diffs.",
  },
  {
    token: "sha256",
    title: "Hashes, not tags",
    body: "A package is its bytes. The lockfile proves exactly what reaches every host.",
  },
  {
    token: "execve",
    title: "Processes, not daemons",
    body: "Foreground by default. Signals, stdout, and exit codes behave the way Unix expects.",
  },
];

const LOOP = [
  {
    verb: "Resolve",
    command: "ply build .",
    body: "Write ply.lock and emit one deterministic .img.",
  },
  {
    verb: "Move",
    command: "scp myapp-*.img server:",
    body: "The deploy artifact is one file. Any file host works.",
  },
  {
    verb: "Execute",
    command: "ssh server ply run myapp-*.img",
    body: "Fetch the locked closure by hash, mount it, exec the entrypoint.",
  },
];

export default function Home() {
  return (
    <main className="mx-auto w-full max-w-6xl px-5 sm:px-7">
      <JsonLd
        data={{
          "@context": "https://schema.org",
          "@type": "SoftwareApplication",
          "@id": `${SITE_URL}/#software`,
          name: "ply",
          description: SITE_DESCRIPTION,
          url: `${SITE_URL}/`,
          downloadUrl: `${SITE_URL}/install.sh`,
          codeRepository: GITHUB_URL,
          applicationCategory: "DeveloperApplication",
          applicationSubCategory: "Container runtime and package manager",
          operatingSystem: "Linux",
          softwareRequirements: "Linux on x86_64 or arm64",
          license: `${GITHUB_URL}/blob/main/LICENSE`,
          offers: {
            "@type": "Offer",
            price: "0",
            priceCurrency: "USD",
          },
        }}
      />
      <section className="grid gap-12 py-16 sm:py-20 lg:grid-cols-[1.05fr_0.95fr] lg:items-center lg:gap-16 lg:py-24">
        <div>
          <p className="eyebrow">npm for containers</p>
          <h1 className="mt-5 max-w-3xl text-[clamp(2.9rem,7vw,4.9rem)] font-medium leading-[0.94] tracking-[-0.055em]">
            Ship one file.
            <span className="mt-2 block text-fade text-[68px]">Run like a process.</span>
          </h1>
          <p className="mt-6 max-w-xl text-lg leading-8 text-fade">
            ply resolves your app and its OS dependencies, mounts the exact
            closure, and execs your entrypoint. No daemon, no Dockerfile, no
            registry server.
          </p>

          <div className="utility-surface mt-8 flex max-w-xl items-stretch border border-edge">
            <code className="min-w-0 flex-1 overflow-x-auto px-4 py-3.5 font-mono text-[13px] leading-5 text-ink">
              {INSTALL_COMMAND}
            </code>
            <CopyButton value={INSTALL_COMMAND} label="copy" className="joined-control shrink-0 border-y-0 border-r-0" />
          </div>

          <div className="mt-6 flex flex-wrap gap-3 font-mono text-[13px]">
            <Link
              href="/docs/quickstart/"
              className="primary-action inline-flex min-h-11 items-center border border-accent bg-accent px-4 text-ground transition-colors hover:bg-transparent hover:text-accent"
            >
              start with ply →
            </Link>
            <Link
              href="/registry/"
              className="secondary-action inline-flex min-h-11 items-center border border-edge px-4 text-fade transition-colors hover:border-accent hover:text-accent"
            >
              browse packages
            </Link>
          </div>
        </div>

        <HeroSlides />
      </section>

      <section className="border-y border-edge py-14 sm:py-16" aria-labelledby="principles-title">
        <div className="grid gap-8 lg:grid-cols-[0.8fr_2.2fr] lg:gap-14">
          <div>
            <p className="eyebrow">the model</p>
            <h2 id="principles-title" className="mt-3 text-2xl font-medium tracking-[-0.025em]">
              Three rules,
              <span className="block text-fade">all the way down.</span>
            </h2>
          </div>
          <dl className="ply-panel grid gap-px border border-edge bg-edge md:grid-cols-3">
            {PRINCIPLES.map((item) => (
              <div key={item.token} className="bg-ground p-5 sm:p-6">
                <dt>
                  <span className="font-mono text-[10px] uppercase tracking-wider text-accent">{item.token}</span>
                  <span className="mt-4 block text-lg font-medium tracking-tight text-ink">{item.title}</span>
                </dt>
                <dd className="mt-2 text-sm leading-6 text-fade">{item.body}</dd>
              </div>
            ))}
          </dl>
        </div>
      </section>

      <section className="py-16 sm:py-20" aria-labelledby="loop-title">
        <div className="max-w-2xl">
          <p className="eyebrow">the whole loop</p>
          <h2 id="loop-title" className="mt-3 text-3xl font-medium tracking-[-0.035em] sm:text-4xl">
            Build. Move. Run.
          </h2>
          <p className="mt-4 text-base leading-7 text-fade">
            Composition is a package-manager problem. Deployment stays a file-transfer problem.
          </p>
        </div>

        <ol className="ply-panel mt-9 grid gap-px border border-edge bg-edge lg:grid-cols-3">
          {LOOP.map((step, index) => (
            <li key={step.verb} className="bg-card p-5 sm:p-6">
              <div className="flex items-center justify-between font-mono text-[10px] uppercase tracking-wider">
                <span className="text-accent">0{index + 1}</span>
                <span className="text-fade">{step.verb}</span>
              </div>
              <code className="mt-8 block overflow-x-auto whitespace-nowrap font-mono text-sm text-ink">
                {step.command}
              </code>
              <p className="mt-3 text-sm leading-6 text-fade">{step.body}</p>
            </li>
          ))}
        </ol>

        <p className="mt-6 max-w-3xl text-sm leading-6 text-fade">
          Think “the SQLite of containers”: one static binary, no build cache, no
          orchestrator. Read the{" "}
          <Link href="/docs/ply-vs-docker/" className="text-accent hover:underline">
            honest comparison with Docker
          </Link>
          —including when ply is the wrong tool.
        </p>
      </section>
    </main>
  );
}
