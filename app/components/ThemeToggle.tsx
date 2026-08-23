"use client";
import { useSyncExternalStore } from "react";

type Theme = "light" | "dark";

const THEME_EVENT = "ply-theme-change";

function currentTheme(): Theme {
  const chosen = document.documentElement.dataset.theme;
  if (chosen === "light" || chosen === "dark") return chosen;
  return matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

function subscribe(onChange: () => void) {
  const system = matchMedia("(prefers-color-scheme: light)");
  system.addEventListener("change", onChange);
  window.addEventListener(THEME_EVENT, onChange);
  return () => {
    system.removeEventListener("change", onChange);
    window.removeEventListener(THEME_EVENT, onChange);
  };
}

// Three states: unset (follow system), "light", "dark". The toggle flips
// relative to what's currently SHOWING, and persists the explicit choice.
export function ThemeToggle() {
  const showing = useSyncExternalStore(subscribe, currentTheme, () => "dark");

  const flip = () => {
    const next = showing === "dark" ? "light" : "dark";
    document.documentElement.dataset.theme = next;
    try { localStorage.setItem("theme", next); } catch {}
    window.dispatchEvent(new Event(THEME_EVENT));
  };

  return (
    <button
      type="button"
      onClick={flip}
      aria-label={`switch to ${showing === "dark" ? "light" : "dark"} mode`}
      title={`switch to ${showing === "dark" ? "light" : "dark"} mode`}
      className="inline-flex size-11 cursor-pointer items-center justify-center text-fade transition-colors hover:text-accent"
    >
      {showing === "dark" ? (
        <svg aria-hidden="true" viewBox="0 0 24 24" className="size-[18px]" fill="none" stroke="currentColor" strokeWidth="1.7">
          <circle cx="12" cy="12" r="3.5" />
          <path d="M12 2v2M12 20v2M4.93 4.93l1.42 1.42M17.65 17.65l1.42 1.42M2 12h2M20 12h2M4.93 19.07l1.42-1.42M17.65 6.35l1.42-1.42" />
        </svg>
      ) : (
        <svg aria-hidden="true" viewBox="0 0 24 24" className="size-[18px]" fill="none" stroke="currentColor" strokeWidth="1.7">
          <path d="M20.2 15.2A8.5 8.5 0 0 1 8.8 3.8 8.5 8.5 0 1 0 20.2 15.2Z" />
        </svg>
      )}
    </button>
  );
}
