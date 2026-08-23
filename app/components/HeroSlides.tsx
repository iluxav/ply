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
  { name: APP, version: "1.4.0", hash: "9b72…f31a", role: "app" },
  { name: "ffmpeg", version: "6.1.1", hash: "64e8…a210", role: "dependency" },
  { name: "python3", version: "3.12.13", hash: "2c1f…8d09", role: "dependency" },
  { name: "alpine", version: "3.20.7", hash: "a44c…e781", role: "base" },
];

const SLIDES = [
  { id: "files", label: "project files", caption: `${APP}/`, badge: "python" },
  { id: "manifest", label: "ply.toml", caption: "ply.toml", badge: `${MANIFEST.split("\n").length} lines` },
  { id: "run", label: "build and run", caption: "terminal", badge: "3 instances" },
  { id: "deploy", label: "rolling deploy", caption: "deploy", badge: "zero downtime" },
  { id: "closure", label: "resolved closure", caption: "resolved closure", badge: "verified" },
] as const;

const INTERVAL_MS = 5000;

function FilesSlide() {
  return (
    <div className="p-4 font-mono text-sm sm:p-5">
      <p className="text-ink">{APP}/</p>
      <ul className="mt-1">
        {FILES.map((f, i) => (
          <li key={f.name} className="flex items-baseline gap-3 leading-7">
            <span className="text-fade">{i === FILES.length - 1 ? "└──" : "├──"}</span>
            <span className="text-ink">{f.name}</span>
            {f.note && <span className="ml-auto text-right text-xs text-fade">{f.note}</span>}
          </li>
        ))}
      </ul>
      <p className="mt-5 border-t border-edge pt-4 font-sans text-xs leading-5 text-fade">
        ply.toml is what you declare; ply.lock is what ply proved. Commit both.
      </p>
    </div>
  );
}

function ManifestSlide() {
  return (
    <pre className="whitespace-pre-wrap [overflow-wrap:anywhere] p-4 font-mono text-[13px] leading-6 sm:p-5">
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
    <div className="p-4 sm:p-5">
      <pre className="whitespace-pre-wrap [overflow-wrap:anywhere] font-mono text-[13px] leading-6"><code>
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
    <>
      <div className="border-b border-edge bg-ground/50 px-4 py-4">
        <p className="font-mono text-sm text-ink">{IMG}</p>
        <p className="mt-1 font-mono text-[10px] uppercase tracking-wider text-fade">
          one file · {SIZE}
        </p>
      </div>
      <div className="p-4 sm:p-5">
        <p className="font-mono text-[10px] uppercase tracking-[0.16em] text-fade">
          mounted lowerdirs
        </p>
        <p className="mt-1 font-mono text-[10px] text-fade">
          app in the image · dependencies by hash from your sources
        </p>
        <ol className="mt-3 space-y-2.5">
          {CLOSURE.map((pkg) => (
            <li key={pkg.name} className="closure-line flex gap-3">
              <span className="relative z-10 mt-3 size-[9px] shrink-0 rounded-full border border-accent bg-card" />
              <div className="flex min-w-0 flex-1 items-center justify-between gap-3 rounded-[4px] border border-edge bg-ground px-3 py-2 font-mono">
                <div className="min-w-0">
                  <span className="text-sm text-ink">{pkg.name}</span>
                  <span className="ml-1.5 text-xs text-fade">@{pkg.version}</span>
                </div>
                <div className="flex shrink-0 items-center gap-2 text-[10px] text-fade">
                  <span className="hidden sm:inline">{pkg.hash}</span>
                  <span className="border border-edge px-1.5 py-0.5">{pkg.role}</span>
                </div>
              </div>
            </li>
          ))}
        </ol>
      </div>
    </>
  );
}

const BODIES = [FilesSlide, ManifestSlide, RunSlide, DeploySlide, ClosureSlide];

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
    <figure
      className="hero-box min-w-0 border border-edge bg-card lg:mt-5"
      onMouseEnter={() => setPaused(true)}
      onMouseLeave={() => setPaused(false)}
      onFocus={() => setPaused(true)}
      onBlur={() => setPaused(false)}
    >
      <figcaption className="flex items-center justify-between border-b border-edge px-4 py-3 font-mono text-[11px] text-fade">
        <span>{slide.caption}</span>
        <span className="flex items-center gap-2 text-accent">
          <span className="size-1.5 rounded-full bg-accent" /> {slide.badge}
        </span>
      </figcaption>

      <div className="grid grid-cols-[minmax(0,1fr)]">
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
              className={`col-start-1 row-start-1 min-w-0 transition-opacity duration-150 ${active ? "opacity-100" : "invisible opacity-0"}`}
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
        className="flex items-center justify-center border-t border-edge px-2 py-1"
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
              aria-label={s.label}
              tabIndex={active ? 0 : -1}
              onClick={() => select(i)}
              className="group flex size-8 items-center justify-center"
            >
              <span
                className={`block h-1.5 rounded-full transition-all duration-150 ${active ? "w-4 bg-accent" : "w-1.5 bg-edge group-hover:bg-fade"}`}
              />
            </button>
          );
        })}
      </div>
    </figure>
  );
}
