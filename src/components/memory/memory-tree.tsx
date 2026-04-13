import { useEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties } from "react";
import { FileText, Folder, FolderOpen, Hash } from "lucide-react";
import { Tree, type NodeRendererProps } from "react-arborist";

import type { PageSummaryDto } from "@/lib/bindings";
import { cn } from "@/lib/utils";

type MemoryTreeProps = {
  pages: PageSummaryDto[];
  selectedPath: string | null;
  onSelectPath: (path: string) => void;
};

type MemoryTreeNode = {
  id: string;
  name: string;
  kind: "group" | "page";
  path?: string;
  children?: MemoryTreeNode[];
};

const PAGE_GROUPS: Array<{ key: string; label: string }> = [
  { key: "index", label: "Index" },
  { key: "topic", label: "Topics" },
  { key: "entity", label: "Entities" },
  { key: "decision", label: "Decisions" },
  { key: "skill", label: "Skills" },
  { key: "source", label: "Sources" },
  { key: "schema", label: "Schemas" },
  { key: "log", label: "Logs" },
];

/**
 * Virtualized tree for grouped workspace memory pages.
 */
export function MemoryTree({
  pages,
  selectedPath,
  onSelectPath,
}: MemoryTreeProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [dimensions, setDimensions] = useState({ height: 480, width: 248 });

  useEffect(() => {
    const element = containerRef.current;
    if (!element) {
      return;
    }

    const updateDimensions = () => {
      setDimensions({
        height: Math.max(240, Math.floor(element.clientHeight)),
        width: Math.max(180, Math.floor(element.clientWidth)),
      });
    };

    updateDimensions();

    const observer = new ResizeObserver(updateDimensions);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const treeData = useMemo(() => buildTreeData(pages), [pages]);

  return (
    <div className="min-h-0 flex-1" ref={containerRef}>
      <Tree<MemoryTreeNode>
        data={treeData}
        height={dimensions.height}
        idAccessor="id"
        indent={18}
        onActivate={(node) => {
          if (node.data.kind === "page" && node.data.path) {
            onSelectPath(node.data.path);
          }
        }}
        openByDefault
        overscanCount={6}
        rowHeight={34}
        selection={selectedPath ?? undefined}
        width={dimensions.width}
      >
        {MemoryTreeRow}
      </Tree>
    </div>
  );
}

function buildTreeData(pages: PageSummaryDto[]): MemoryTreeNode[] {
  return PAGE_GROUPS.map((group) => {
    const groupPages = pages
      .filter((page) => page.pageType === group.key)
      .sort((left, right) => left.title.localeCompare(right.title));

    return {
      id: `group:${group.key}`,
      name: group.label,
      kind: "group" as const,
      children: groupPages.map((page) => ({
        id: page.path,
        kind: "page" as const,
        name: page.title || page.path,
        path: page.path,
      })),
    };
  }).filter((group) => group.children?.length);
}

function MemoryTreeRow({
  node,
  style,
  dragHandle,
}: NodeRendererProps<MemoryTreeNode>) {
  const isGroup = node.data.kind === "group";

  return (
    <div
      className="px-2"
      ref={dragHandle}
      style={style as CSSProperties}
    >
      <div
        className={cn(
          "flex h-8 items-center gap-2 rounded-md px-2 text-sm transition",
          node.isSelected
            ? "bg-accent text-accent-foreground"
            : "text-muted-foreground hover:bg-accent/40 hover:text-foreground",
        )}
        onClick={node.handleClick}
        role="button"
        tabIndex={-1}
      >
        {isGroup ? (
          <button
            className="flex size-5 items-center justify-center rounded-sm text-muted-foreground hover:bg-accent"
            onClick={(event) => {
              event.stopPropagation();
              node.toggle();
            }}
            type="button"
          >
            {node.isOpen ? (
              <FolderOpen className="h-3.5 w-3.5" />
            ) : (
              <Folder className="h-3.5 w-3.5" />
            )}
          </button>
        ) : (
          <span className="flex size-5 items-center justify-center text-muted-foreground">
            <FileText className="h-3.5 w-3.5" />
          </span>
        )}

        <span className="min-w-0 flex-1 truncate">
          {node.data.name}
        </span>

        {isGroup ? (
          <Hash className="h-3 w-3 shrink-0 text-muted-foreground/70" />
        ) : null}
      </div>
    </div>
  );
}
