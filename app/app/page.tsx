import Link from "next/link";

const FEATURES: [string, string][] = [
  ["no daemon", "one static binary; the kernel is the only thing resident"],
  ["deterministic images", "same input → byte-identical squashfs, content-addressed"],
  ["npm-like deps", "manifest + lockfile + version ranges; kegs never conflict"],
  ["any file host is a registry", "GitHub Releases, a bucket, a directory — zero API"],
  ["zero-downtime deploys", "health-gated rolling deploys via a single SIGHUP"],
  ["scale built in", "--scale N instances, --publish load-balances the pool"],
];

export default function Home() {
  return (
    <main>
      <section className="pt-20 pb-12">
        <h1 className="text-4xl tracking-tight">
          npm for containers<span className="cursor-blink text-accent">▍</span>
        </h1>
        <p className="mt-4 max-w-2xl text-fade leading-7">
          Your app is a package, its OS-level dependencies are packages, an
          image is a resolved lockfile — and the runtime is a boring static
          binary that mounts the closure and execs your entrypoint.
        </p>
        <div className="mt-8 border border-edge bg-card px-4 py-3 max-w-xl">
          <pre className="overflow-x-auto text-sm leading-6"><code>
            <span className="text-fade"># one file lands on the host</span>{"\n"}
            curl -fsSL https://plybox.sh/install.sh | sh
          </code></pre>
        </div>
        <div className="mt-6 flex gap-4 text-sm">
          <Link href="/docs/quickstart/" className="border border-accent text-accent px-4 py-2 hover:bg-deep">
            quickstart →
          </Link>
          <Link href="/registry/" className="border border-edge text-fade px-4 py-2 hover:text-accent hover:border-accent">
            browse packages
          </Link>
        </div>
      </section>

      <section className="py-10 grid gap-px sm:grid-cols-2 lg:grid-cols-3 border border-edge bg-edge">
        {FEATURES.map(([title, body]) => (
          <div key={title} className="bg-ground p-5">
            <h2 className="text-accent text-sm">{title}</h2>
            <p className="mt-2 text-xs text-fade leading-5">{body}</p>
          </div>
        ))}
      </section>

      <section className="py-10">
        <h2 className="text-xl tracking-tight">the whole loop</h2>
        <div className="mt-4 border border-edge bg-card px-4 py-3 max-w-2xl">
          <pre className="overflow-x-auto text-sm leading-6"><code>
            ply build .                    <span className="text-fade"># resolve deps, write ply.lock, emit one .img</span>{"\n"}
            scp myapp-*.img server:        <span className="text-fade"># the deploy artifact is one file</span>{"\n"}
            ssh server ply run myapp-*.img <span className="text-fade"># mounts the closure, execs your app</span>
          </code></pre>
        </div>
        <p className="mt-4 text-sm text-fade max-w-2xl leading-6">
          Think “the SQLite of containers.” No registry server, no Dockerfile,
          no build cache, no orchestrator — and{" "}
          <Link href="/docs/ply-vs-docker/" className="text-accent hover:underline">
            an honest comparison with Docker
          </Link>{" "}
          for when you should not use ply.
        </p>
      </section>
    </main>
  );
}
