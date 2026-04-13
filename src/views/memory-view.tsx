import { useEffect, useState } from "react";
import { useMutation, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { BookMarked } from "lucide-react";

import type { WikiPageDto } from "@/lib/bindings";
import { MemoryEditor } from "@/components/memory/memory-editor";
import { MemoryPageViewer } from "@/components/memory/memory-page-viewer";
import { MemorySearch } from "@/components/memory/memory-search";
import { MemoryTree } from "@/components/memory/memory-tree";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { useMemoryPage, useMemoryPages, useMemorySearch } from "@/hooks/use-memory-pages";
import { tauriClient } from "@/lib/tauri";

/**
 * Two-pane memory browser with search, editing, creation, and deletion.
 */
export function MemoryView() {
  const queryClient = useQueryClient();
  const [searchQuery, setSearchQuery] = useState("");
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [editorDraft, setEditorDraft] = useState<WikiPageDto | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<WikiPageDto | null>(null);

  const pages = useMemoryPages(null);
  const search = useMemorySearch(searchQuery);
  const selectedPage = useMemoryPage(selectedPath);

  useEffect(() => {
    if (editorDraft) {
      return;
    }

    const allPages = pages.data ?? [];
    if (!allPages.length) {
      setSelectedPath(null);
      return;
    }

    if (!selectedPath || !allPages.some((page) => page.path === selectedPath)) {
      setSelectedPath(allPages[0]?.path ?? null);
    }
  }, [editorDraft, pages.data, selectedPath]);

  const savePage = useMutation({
    mutationFn: (page: WikiPageDto) => tauriClient.writeMemoryPage(normalizePageForSave(page)),
    onSuccess: async (page) => {
      await invalidateMemoryQueries(queryClient);
      setEditorDraft(null);
      setSearchQuery("");
      setSelectedPath(page.path);
    },
  });

  const deletePage = useMutation({
    mutationFn: (path: string) => tauriClient.deleteMemoryPage(path),
    onSuccess: async (_value, path) => {
      await invalidateMemoryQueries(queryClient);
      setDeleteTarget(null);
      setEditorDraft(null);
      if (selectedPath === path) {
        setSelectedPath(null);
      }
    },
  });

  const currentPage = editorDraft ?? selectedPage.data ?? null;
  const showingSearchResults = searchQuery.trim().length >= 2;

  const handleSelectPath = (path: string) => {
    setEditorDraft(null);
    setSearchQuery("");
    setSelectedPath(path);
  };

  const handleCreatePage = () => {
    setEditorDraft(createDraftPage());
  };

  return (
    <>
      <div className="flex h-full min-h-0">
        <div className="flex w-[250px] shrink-0 flex-col border-r border-border">
          <MemorySearch
            isLoading={search.isLoading}
            onCreatePage={handleCreatePage}
            onQueryChange={setSearchQuery}
            onSelectPath={handleSelectPath}
            query={searchQuery}
            results={search.data ?? []}
          />

          {!showingSearchResults ? (
            pages.isLoading ? (
              <div className="px-4 py-6 text-sm text-muted-foreground">Loading pages…</div>
            ) : pages.data?.length ? (
              <MemoryTree
                onSelectPath={handleSelectPath}
                pages={pages.data}
                selectedPath={selectedPath}
              />
            ) : (
              <EmptyBrowserState onCreatePage={handleCreatePage} />
            )
          ) : null}
        </div>

        <div className="min-w-0 flex-1 overflow-hidden">
          {editorDraft ? (
            <MemoryEditor
              isSaving={savePage.isPending}
              onCancel={() => setEditorDraft(null)}
              onDelete={editorDraft.path ? () => setDeleteTarget(editorDraft) : undefined}
              onSave={(page) => savePage.mutate(page)}
              page={editorDraft}
            />
          ) : currentPage ? (
            <MemoryPageViewer
              onDelete={() => setDeleteTarget(currentPage)}
              onEdit={() => setEditorDraft(currentPage)}
              onNavigate={handleSelectPath}
              page={currentPage}
              pages={pages.data ?? []}
            />
          ) : (
            <EmptyBrowserState onCreatePage={handleCreatePage} />
          )}
        </div>
      </div>

      <AlertDialog onOpenChange={(open) => !open && setDeleteTarget(null)} open={Boolean(deleteTarget)}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Delete memory page?</AlertDialogTitle>
            <AlertDialogDescription>
              {deleteTarget?.title
                ? `This will permanently remove ${deleteTarget.title} from the workspace memory browser.`
                : "This page will be permanently removed from the workspace memory browser."}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction
              onClick={() => {
                if (deleteTarget?.path) {
                  deletePage.mutate(deleteTarget.path);
                }
              }}
            >
              {deletePage.isPending ? "Deleting…" : "Delete"}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}

function createDraftPage(): WikiPageDto {
  const now = new Date().toISOString();

  return {
    autoGenerated: false,
    confidence: "medium",
    content: "",
    created: now,
    lastReferenced: now,
    metadata: {},
    pageType: "topic",
    path: null,
    referenceCount: 0,
    related: [],
    sources: [],
    tags: [],
    title: "",
    updated: now,
  };
}

function normalizePageForSave(page: WikiPageDto): WikiPageDto {
  const now = new Date().toISOString();
  const normalizedTitle = page.title.trim() || "Untitled page";
  const normalizedPageType = page.pageType || "topic";
  const normalizedConfidence = page.confidence || "medium";
  const explicitPath = page.path?.trim();
  const derivedPath =
    normalizedPageType === "index"
      ? "MEMORY.md"
      : `${pageTypeFolder(normalizedPageType)}/${slugify(normalizedTitle) || "untitled-page"}.md`;

  return {
    ...page,
    confidence: normalizedConfidence,
    created: page.created || now,
    lastReferenced: page.lastReferenced || now,
    pageType: normalizedPageType,
    path: explicitPath || derivedPath,
    title: normalizedTitle,
    updated: now,
  };
}

async function invalidateMemoryQueries(queryClient: QueryClient) {
  await Promise.all([
    queryClient.invalidateQueries({ queryKey: ["memory-page"] }),
    queryClient.invalidateQueries({ queryKey: ["memory-pages"] }),
    queryClient.invalidateQueries({ queryKey: ["memory-search"] }),
  ]);
}

function pageTypeFolder(pageType: string): string {
  switch (pageType) {
    case "entity":
      return "entities";
    case "decision":
      return "decisions";
    case "skill":
      return "skills";
    case "source":
      return "sources";
    case "schema":
      return "schemas";
    case "log":
      return "logs";
    case "topic":
    default:
      return "topics";
  }
}

function slugify(value: string): string {
  return value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

function EmptyBrowserState({
  onCreatePage,
}: {
  onCreatePage: () => void;
}) {
  return (
    <div className="flex h-full items-center justify-center px-8">
      <div className="max-w-md text-center">
        <BookMarked className="mx-auto h-10 w-10 text-muted-foreground" />
        <h2 className="mt-4 text-lg font-semibold">Memory browser</h2>
        <p className="mt-2 text-sm text-muted-foreground">
          Browse, search, and edit workspace knowledge pages from this view.
        </p>
        <Button className="mt-4" onClick={onCreatePage} type="button" variant="secondary">
          Create page
        </Button>
      </div>
    </div>
  );
}
