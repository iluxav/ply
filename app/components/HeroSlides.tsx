"use client";
import { useEffect, useId, useRef, useState, type KeyboardEvent } from "react";

// One example app, carried through every slide: versions here must match.
const APP = "myapp";
const IMG = "myapp-1.4.0-linux-x64.img";
const SIZE = "3.8 MiB";

const FILES = [
  { name: "app.py", note: "" },
  { name: "requirements.txt", note: "" },
  { name: "ply.toml", note: "you write this" },
  { name: "ply.lock", note: "ply build writes this" },
];

const MANIFEST = `[package]
name = "myapp"
version = "1.4.0"
entrypoint = ["python3", "app.py"]
base = "alpine@3.20"

[dependencies]
python3 = "3.12"
ffmpeg = "6.1"

[ports]
http = 8000

[health]
port = 8000

[sources]
default = "https://registry.plybox.sh/ply/{package}"`;

type TermLine = { kind: "cmd" | "out" | "row" | "comment" | "gap"; text?: string };

// `locked …` / `built …` are the lines ply build prints (docs/quickstart);
// `ply ps` columns follow ply-cli/src/commands/ps.rs.
const RUN: TermLine[] = [
  { kind: "cmd", text: "ply build ." },
  { kind: "out", text: "locked alpine 3.20.7, python3 3.12.13, ffmpeg 6.1.1" },
  { kind: "out", text: `built ${IMG} (${SIZE})` },
  { kind: "gap" },
  { kind: "cmd", text: `ply run --publish 8000 --scale 3 ${IMG}` },
  { kind: "gap" },
  { kind: "comment", text: "# another terminal" },
  { kind: "cmd", text: "ply ps" },
  { kind: "out", text: "NAME      PORTS       UPTIME  STATUS" },
  { kind: "row", text: "myapp.1   http:8000   12s     up" },
  { kind: "row", text: "myapp.2   http:8000   12s     up" },
  { kind: "row", text: "myapp.3   http:8000   12s     up" },
];

// The `ply:` lines are the run parent's roll messages (ply-core/src/runtime/run.rs).
const NEXT_IMG = "myapp-1.5.0-linux-x64.img";
const DEPLOY: TermLine[] = [
  { kind: "cmd", text: "ply build ." },
  { kind: "out", text: `built ${NEXT_IMG} (${SIZE})` },
  { kind: "gap" },
  { kind: "cmd", text: `ply deploy ${NEXT_IMG}` },
  { kind: "out", text: `ply: deploy -> ${NEXT_IMG}` },
  { kind: "row", text: `ply: myapp.1 now on ${NEXT_IMG}` },
  { kind: "row", text: `ply: myapp.2 now on ${NEXT_IMG}` },
  { kind: "row", text: `ply: myapp.3 now on ${NEXT_IMG}` },
  { kind: "out", text: `ply: deploy complete — all instances on ${NEXT_IMG}` },
];

const CLOSURE = [
  { name: APP, version: "1.4.0", hash: "9b72…f31a", role: "app", location: "image" },
  { name: "ffmpeg", version: "6.1.1", hash: "64e8…a210", role: "dep", location: "store" },
  { name: "python3", version: "3.12.13", hash: "2c1f…8d09", role: "runtime", location: "store" },
  { name: "alpine", version: "3.20.7", hash: "a44c…e781", role: "base", location: "store" },
];

const SLIDES = [
  { id: "closure", label: "artifact", caption: "resolved closure", badge: "verified" },
  { id: "files", label: "project", caption: `${APP}/`, badge: "4 files" },
  { id: "manifest", label: "manifest", caption: "ply.toml", badge: `${MANIFEST.split("\n").length} lines` },
  { id: "run", label: "run ×3", caption: "terminal", badge: "3 instances" },
  { id: "deploy", label: "deploy", caption: "rolling deploy", badge: "healthy" },
] as const;

const INTERVAL_MS = 6000;

function FilesSlide() {
  return (
    <div className="p-5 font-mono text-sm sm:p-6">
      <p className="text-ink">{APP}/</p>
      <ul className="mt-2">
        {FILES.map((f, index) => (
          <li key={f.name} className="flex items-baseline gap-3 leading-8">
            <span className="text-fade">{index === FILES.length - 1 ? "└──" : "├──"}</span>
            <span className={f.note ? "text-ink" : "text-fade"}>{f.name}</span>
            {f.note && <span className="ml-auto text-right text-[10px] text-fade">{f.note}</span>}
          </li>
        ))}
      </ul>
      <p className="mt-6 border-t border-edge pt-4 font-sans text-xs leading-5 text-fade">
        You declare ply.toml. ply build resolves the graph and writes ply.lock.
      </p>
    </div>
  );
}

function ManifestSlide() {
  return (
    <pre className="whitespace-pre-wrap [overflow-wrap:anywhere] p-5 font-mono text-[12px] leading-[1.65] sm:p-6">
      <code>
        {MANIFEST.split("\n").map((line, i) => {
          const eq = line.indexOf(" = ");
          if (line.startsWith("[")) return <span key={i} className="block text-fade">{line}</span>;
          if (eq === -1) return <span key={i} className="block">{line || " "}</span>;
          return (
            <span key={i} className="block">
              <span className="text-ink">{line.slice(0, eq)}</span>
              <span className="text-fade"> = </span>
              <span className="text-accent">{line.slice(eq + 3)}</span>
            </span>
          );
        })}
      </code>
    </pre>
  );
}

function Term({ lines, note }: { lines: TermLine[]; note: string }) {
  return (
    <div className="p-5 sm:p-6">
      <pre className="whitespace-pre-wrap [overflow-wrap:anywhere] font-mono text-[12px] leading-[1.65]"><code>
        {lines.map((line, i) =>
          line.kind === "gap" ? (
            <span key={i} className="block"> </span>
          ) : line.kind === "cmd" ? (
            <span key={i} className="block text-ink">
              <span className="text-accent">$ </span>
              {line.text}
            </span>
          ) : (
            <span
              key={i}
              className={`block ${line.kind === "row" ? "text-ink" : line.kind === "comment" ? "text-fade/70" : "text-fade"}`}
            >
              {line.text}
            </span>
          ),
        )}
      </code></pre>
      <p className="mt-5 border-t border-edge pt-4 text-xs leading-5 text-fade">{note}</p>
    </div>
  );
}

const RunSlide = () => (
  <Term lines={RUN} note="One host port, L4-balanced across three instances — no proxy, no root." />
);
const DeploySlide = () => (
  <Term
    lines={DEPLOY}
    note="Instances roll one at a time behind a health gate; a failed gate reverts that slot and leaves the rest on 1.4.0."
  />
);

function ClosureSlide() {
  return (
    <div className="p-5 sm:p-6">
      <div className="artifact-ticket">
        <div className="min-w-0">
          <p className="font-mono text-[10px] uppercase tracking-[0.18em] text-accent">build output</p>
          <p className="mt-1.5 truncate font-mono text-sm text-ink">{IMG}</p>
        </div>
        <div className="shrink-0 text-right font-mono">
          <p className="text-sm text-ink">{SIZE}</p>
          <p className="mt-1 text-[9px] uppercase tracking-[0.16em] text-fade">squashfs</p>
        </div>
      </div>

      <div className="mt-5 flex items-end justify-between gap-3 font-mono">
        <div>
          <p className="text-[10px] uppercase tracking-[0.16em] text-fade">mounted closure</p>
          <p className="mt-1 text-[10px] text-fade">one version per package</p>
        </div>
        <p className="text-[10px] text-accent">4 / 4 digests match</p>
      </div>

      <ol className="mt-3 space-y-1.5">
        {CLOSURE.map((pkg, index) => (
          <li key={pkg.name} className="closure-layer grid grid-cols-[auto_minmax(0,1fr)_auto] items-center gap-3 border border-edge bg-ground px-3 py-2 font-mono">
            <span className="text-[9px] text-fade">{String(index + 1).padStart(2, "0")}</span>
            <div className="min-w-0">
              <span className="text-xs text-ink">{pkg.name}</span>
              <span className="ml-1.5 text-[10px] text-fade">@{pkg.version}</span>
            </div>
            <div className="flex items-center gap-2 text-[9px] uppercase tracking-wider text-fade">
              <span className="hidden sm:inline">{pkg.hash}</span>
              <span className={pkg.location === "image" ? "text-accent" : ""}>{pkg.location}</span>
              <span className="w-12 border-l border-edge pl-2 text-right">{pkg.role}</span>
            </div>
          </li>
        ))}
      </ol>

      <div className="mt-4 flex items-center gap-3 border-t border-edge pt-4 font-mono text-[10px] text-fade">
        <span className="inline-flex items-center gap-2 text-accent">
          <span className="size-1.5 bg-accent" /> exact bytes
        </span>
        <span aria-hidden="true">→</span>
        <span>one mounted root</span>
      </div>
    </div>
  );
}

const BODIES = [ClosureSlide, FilesSlide, ManifestSlide, RunSlide, DeploySlide];

function PlyMark() {
  return (
    <svg aria-hidden="true" viewBox="0 0 64 64" className="size-8 shrink-0">
      <path
        d="M63 1V51A12 12 0 0 1 51 63H13A12 12 0 0 1 1 51V13A12 12 0 0 1 13 1Z"
        fill="var(--color-ground)"
        stroke="var(--color-edge)"
        strokeWidth="2"
      />
      <text
        x="32"
        y="41"
        fill="var(--color-accent)"
        fontFamily="ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
        fontSize="26"
        textAnchor="middle"
      >
        ply
      </text>
    </svg>
  );
}

export function HeroSlides() {
  const [index, setIndex] = useState(0);
  const [paused, setPaused] = useState(false);
  const [manual, setManual] = useState(false);
  const tabs = useRef<(HTMLButtonElement | null)[]>([]);
  const baseId = useId();

  useEffect(() => {
    if (paused || manual) return;
    const reduced = matchMedia("(prefers-reduced-motion: reduce)");
    if (reduced.matches) return;
    const tick = () => {
      if (document.visibilityState === "visible") setIndex((i) => (i + 1) % SLIDES.length);
    };
    const timer = setInterval(tick, INTERVAL_MS);
    const stop = () => clearInterval(timer);
    reduced.addEventListener("change", stop);
    return () => {
      stop();
      reduced.removeEventListener("change", stop);
    };
  }, [paused, manual]);

  const select = (i: number, focus = false) => {
    setManual(true);
    setIndex(i);
    if (focus) tabs.current[i]?.focus();
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const last = SLIDES.length - 1;
    const next =
      e.key === "ArrowRight" ? (index === last ? 0 : index + 1)
      : e.key === "ArrowLeft" ? (index === 0 ? last : index - 1)
      : e.key === "Home" ? 0
      : e.key === "End" ? last
      : null;
    if (next === null) return;
    e.preventDefault();
    select(next, true);
  };

  const slide = SLIDES[index];

  return (
    <div className="hero-stage min-w-0 lg:mt-5">
      <figure
        className="hero-box min-w-0 border border-edge bg-card"
        onMouseEnter={() => setPaused(true)}
        onMouseLeave={() => setPaused(false)}
        onFocus={() => setPaused(true)}
        onBlur={() => setPaused(false)}
      >
        <figcaption className="hero-toolbar flex items-center justify-between gap-4 border-b border-edge px-4 py-3">
          <div className="flex min-w-0 items-center gap-3">
            <PlyMark />
            <div className="min-w-0 font-mono">
              <p className="text-[10px] uppercase tracking-[0.16em] text-fade">ply artifact</p>
              <p className="mt-0.5 truncate text-[11px] text-ink">{slide.caption}</p>
            </div>
          </div>
          <span className="flex shrink-0 items-center gap-2 font-mono text-[10px] uppercase tracking-[0.12em] text-accent">
            <span className="size-1.5 bg-accent" /> {slide.badge}
          </span>
        </figcaption>

        <div className="grid min-h-[386px] grid-cols-[minmax(0,1fr)] sm:min-h-[404px]">
          {BODIES.map((Body, i) => {
            const active = i === index;
            return (
              <div
                key={SLIDES[i].id}
                id={`${baseId}-panel-${i}`}
                role="tabpanel"
                aria-labelledby={`${baseId}-tab-${i}`}
                aria-hidden={!active}
                inert={!active}
                className={`hero-panel col-start-1 row-start-1 min-w-0 ${active ? "is-active" : "invisible"}`}
              >
                <Body />
              </div>
            );
          })}
        </div>

        <div
          role="tablist"
          aria-label="How ply works"
          onKeyDown={onKeyDown}
          className="grid grid-cols-5 border-t border-edge bg-ground/40"
        >
          {SLIDES.map((s, i) => {
            const active = i === index;
            return (
              <button
                key={s.id}
                ref={(el) => { tabs.current[i] = el; }}
                id={`${baseId}-tab-${i}`}
                type="button"
                role="tab"
                aria-selected={active}
                aria-controls={`${baseId}-panel-${i}`}
                tabIndex={active ? 0 : -1}
                onClick={() => select(i)}
                className={`hero-tab relative min-w-0 border-r border-edge px-1 py-3 font-mono text-[9px] uppercase tracking-[0.08em] transition-colors last:border-r-0 sm:px-2 sm:text-[10px] ${
                  active ? "bg-card text-ink" : "text-fade hover:bg-card/60 hover:text-ink"
                }`}
              >
                <span className="block truncate">{s.label}</span>
                <span className={`absolute inset-x-2 bottom-0 h-px ${active ? "bg-accent" : "bg-transparent"}`} />
              </button>
            );
          })}
        </div>
      </figure>

      <div className="hero-proof" aria-hidden="true">
        <span>content addressed</span>
        <span>daemonless</span>
        <span>linux · x64</span>
      </div>
    </div>
  );
}
