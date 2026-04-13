import type { ReactNode } from "react";

import type { PageSummaryDto } from "@/lib/bindings";
import { cn } from "@/lib/utils";

const WIKI_LINK_PREFIX = "memory:";
const WIKI_LINK_PATTERN = /\[\[([^[\]]+)\]\]/g;

/**
 * Converts arbitrary text into a stable slug for memory-path lookups.
 */
function slugify(value: string): string {
  return value
    .trim()
    .toLowerCase()
    .replace(/\.md$/i, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

/**
 * Resolves a wiki-style target to a concrete memory page path when possible.
 */
export function resolveMemoryTarget(
  target: string,
  pages: PageSummaryDto[],
  currentPath?: string | null,
): string | null {
  const raw = target.trim();
  if (!raw) {
    return null;
  }

  const lowerPath = raw.toLowerCase();
  const directMatch = pages.find((page) => page.path.toLowerCase() === lowerPath);
  if (directMatch) {
    return directMatch.path;
  }

  if (!raw.includes("/") && currentPath) {
    const lastSlash = currentPath.lastIndexOf("/");
    const folderPrefix = lastSlash >= 0 ? currentPath.slice(0, lastSlash + 1) : "";
    const sameFolderPath = `${folderPrefix}${raw.endsWith(".md") ? raw : `${raw}.md`}`;
    const sameFolderMatch = pages.find(
      (page) => page.path.toLowerCase() === sameFolderPath.toLowerCase(),
    );
    if (sameFolderMatch) {
      return sameFolderMatch.path;
    }
  }

  const slug = slugify(raw);
  const fuzzyMatch = pages.find((page) => {
    const pageSlug = slugify(page.title);
    const basename = page.path.split("/").pop() ?? page.path;
    return pageSlug === slug || slugify(basename) === slug;
  });

  return fuzzyMatch?.path ?? null;
}

/**
 * Rewrites `[[wiki-links]]` into markdown links using the internal memory scheme.
 */
export function replaceWikiLinks(
  markdown: string,
  resolveTarget: (target: string) => string | null,
): string {
  return markdown.replace(WIKI_LINK_PATTERN, (_match, rawTarget: string) => {
    const target = rawTarget.trim();
    const resolvedTarget = resolveTarget(target) ?? target;
    return `[${target}](${WIKI_LINK_PREFIX}${encodeURIComponent(resolvedTarget)})`;
  });
}

type WikiLinkProps = {
  href?: string | null;
  children: ReactNode;
  className?: string;
  onNavigate?: (path: string) => void;
};

/**
 * Renders markdown links, intercepting internal memory references for in-app navigation.
 */
export function WikiLink({
  href,
  children,
  className,
  onNavigate,
}: WikiLinkProps) {
  if (href?.startsWith(WIKI_LINK_PREFIX)) {
    const target = decodeURIComponent(href.slice(WIKI_LINK_PREFIX.length));
    return (
      <button
        className={cn(
          "text-primary underline underline-offset-4 hover:text-primary/80",
          className,
        )}
        onClick={() => onNavigate?.(target)}
        type="button"
      >
        {children}
      </button>
    );
  }

  return (
    <a
      className={cn(
        "text-primary underline underline-offset-4 hover:text-primary/80",
        className,
      )}
      href={href ?? "#"}
      rel="noreferrer"
      target="_blank"
    >
      {children}
    </a>
  );
}
