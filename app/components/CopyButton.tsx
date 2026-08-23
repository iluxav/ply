"use client";

import { useState } from "react";

type CopyButtonProps = {
  value: string;
  label?: string;
  className?: string;
  iconOnly?: boolean;
};

async function copyText(value: string) {
  if (navigator.clipboard && window.isSecureContext) {
    try {
      await navigator.clipboard.writeText(value);
      return;
    } catch {
      // Fall through for browsers that expose Clipboard API but deny access.
    }
  }

  const field = document.createElement("textarea");
  const active = document.activeElement as HTMLElement | null;
  field.value = value;
  field.setAttribute("readonly", "");
  field.style.position = "fixed";
  field.style.opacity = "0";
  document.body.appendChild(field);
  field.select();
  const copied = document.execCommand("copy");
  field.remove();
  active?.focus();
  if (!copied) throw new Error("Clipboard access unavailable");
}

export function CopyButton({
  value,
  label = "copy",
  className = "",
  iconOnly = false,
}: CopyButtonProps) {
  const [copied, setCopied] = useState(false);

  const copy = async () => {
    try {
      await copyText(value);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1400);
    } catch {
      setCopied(false);
    }
  };

  return (
    <button
      type="button"
      onClick={copy}
      className={`control-shape inline-flex min-h-11 items-center justify-center border border-edge font-mono text-[11px] text-fade transition-colors hover:border-accent hover:text-accent ${
        iconOnly ? "w-11 px-0" : "px-3 md:min-h-8"
      } ${className}`}
      aria-label={`${label} to clipboard`}
    >
      {iconOnly ? (
        <>
          <svg aria-hidden="true" viewBox="0 0 24 24" className="size-4" fill="none" stroke="currentColor" strokeWidth="1.7">
            {copied ? (
              <path d="m5 12 4 4L19 6" />
            ) : (
              <>
                <rect x="8" y="8" width="11" height="11" rx="1" />
                <path d="M16 8V5H5v11h3" />
              </>
            )}
          </svg>
          <span className="sr-only" aria-live="polite">{copied ? "copied" : label}</span>
        </>
      ) : (
        <span aria-live="polite">{copied ? "copied" : label}</span>
      )}
    </button>
  );
}
