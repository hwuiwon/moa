import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Save, Trash2, X } from "lucide-react";

import type { WikiPageDto } from "@/lib/bindings";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";

type MemoryEditorProps = {
  page: WikiPageDto;
  isSaving: boolean;
  onCancel: () => void;
  onDelete?: () => void;
  onSave: (page: WikiPageDto) => void;
};

const PAGE_TYPE_OPTIONS = [
  "topic",
  "entity",
  "decision",
  "skill",
  "source",
  "schema",
  "log",
  "index",
] as const;

const CONFIDENCE_OPTIONS = ["high", "medium", "low"] as const;

/**
 * Inline editor for memory wiki pages.
 */
export function MemoryEditor({
  page,
  isSaving,
  onCancel,
  onDelete,
  onSave,
}: MemoryEditorProps) {
  const [draft, setDraft] = useState(page);
  const [tagsInput, setTagsInput] = useState(page.tags.join(", "));
  const [relatedInput, setRelatedInput] = useState(page.related.join("\n"));
  const [sourcesInput, setSourcesInput] = useState(page.sources.join("\n"));

  useEffect(() => {
    setDraft(page);
    setTagsInput(page.tags.join(", "));
    setRelatedInput(page.related.join("\n"));
    setSourcesInput(page.sources.join("\n"));
  }, [page]);

  const savePreviewPath = useMemo(() => {
    const path = draft.path?.trim();
    if (path) {
      return path;
    }
    return derivePathFromDraft(draft);
  }, [draft]);

  const handleSave = () => {
    onSave({
      ...draft,
      confidence: draft.confidence || "medium",
      pageType: draft.pageType || "topic",
      path: savePreviewPath,
      related: splitEntries(relatedInput),
      sources: splitEntries(sourcesInput),
      tags: splitEntries(tagsInput),
      title: draft.title.trim() || "Untitled page",
    });
  };

  return (
    <div className="h-full overflow-hidden">
      <div className="mx-auto flex h-full max-w-5xl flex-col">
        <div className="flex flex-wrap items-start justify-between gap-4 border-b border-border px-6 py-4">
          <div className="min-w-0">
            <p className="text-[11px] uppercase tracking-widest text-muted-foreground">
              Editor
            </p>
            <h1 className="mt-1 text-xl font-semibold">
              {draft.path ? "Edit memory page" : "Create memory page"}
            </h1>
            <p className="mt-1 text-sm text-muted-foreground">
              Saving writes the markdown page and refreshes the workspace index.
            </p>
          </div>

          <div className="flex items-center gap-2">
            <Button onClick={onCancel} type="button" variant="outline">
              <X className="h-4 w-4" />
              Cancel
            </Button>
            {onDelete ? (
              <Button onClick={onDelete} type="button" variant="outline">
                <Trash2 className="h-4 w-4" />
                Delete
              </Button>
            ) : null}
            <Button disabled={isSaving} onClick={handleSave} type="button">
              <Save className="h-4 w-4" />
              {isSaving ? "Saving…" : "Save"}
            </Button>
          </div>
        </div>

        <div className="grid min-h-0 flex-1 gap-4 overflow-auto px-6 py-6 lg:grid-cols-[minmax(0,320px)_minmax(0,1fr)]">
          <div className="space-y-4">
            <Field label="Title">
              <Input
                onChange={(event) =>
                  setDraft((current) => ({ ...current, title: event.target.value }))
                }
                placeholder="Deployment guide"
                value={draft.title}
              />
            </Field>

            <Field hint={`Will save to ${savePreviewPath}`} label="Path">
              <Input
                onChange={(event) =>
                  setDraft((current) => ({ ...current, path: event.target.value }))
                }
                placeholder="topics/deployment-guide.md"
                value={draft.path ?? ""}
              />
            </Field>

            <div className="grid gap-4 sm:grid-cols-2">
              <Field label="Page type">
                <Select
                  onValueChange={(value) => {
                    if (value) {
                      setDraft((current) => ({ ...current, pageType: value }));
                    }
                  }}
                  value={draft.pageType}
                >
                  <SelectTrigger className="w-full">
                    <SelectValue placeholder="Select page type" />
                  </SelectTrigger>
                  <SelectContent>
                    {PAGE_TYPE_OPTIONS.map((option) => (
                      <SelectItem key={option} value={option}>
                        {option}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </Field>

              <Field label="Confidence">
                <Select
                  onValueChange={(value) => {
                    if (value) {
                      setDraft((current) => ({ ...current, confidence: value }));
                    }
                  }}
                  value={draft.confidence}
                >
                  <SelectTrigger className="w-full">
                    <SelectValue placeholder="Select confidence" />
                  </SelectTrigger>
                  <SelectContent>
                    {CONFIDENCE_OPTIONS.map((option) => (
                      <SelectItem key={option} value={option}>
                        {option}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </Field>
            </div>

            <Field hint="Comma or newline separated" label="Tags">
              <Input
                onChange={(event) => setTagsInput(event.target.value)}
                placeholder="api, auth, deployment"
                value={tagsInput}
              />
            </Field>

            <Field hint="One page path or wiki target per line" label="Related pages">
              <Textarea
                className="min-h-24"
                onChange={(event) => setRelatedInput(event.target.value)}
                placeholder={"entities/auth-service.md\n[[token-rotation]]"}
                value={relatedInput}
              />
            </Field>

            <Field hint="One source URL or page reference per line" label="Sources">
              <Textarea
                className="min-h-24"
                onChange={(event) => setSourcesInput(event.target.value)}
                placeholder={"https://example.com/spec\nsources/rfc-0042.md"}
                value={sourcesInput}
              />
            </Field>
          </div>

          <Field className="min-h-[420px]" label="Markdown content">
            <Textarea
              className="min-h-[420px] font-mono text-sm"
              onChange={(event) =>
                setDraft((current) => ({ ...current, content: event.target.value }))
              }
              placeholder="# Deployment guide"
              value={draft.content}
            />
          </Field>
        </div>
      </div>
    </div>
  );
}

function derivePathFromDraft(page: WikiPageDto): string {
  const folder = pageTypeFolder(page.pageType);
  const slug = slugify(page.title || "untitled-page");
  if (page.pageType === "index") {
    return "MEMORY.md";
  }
  return `${folder}/${slug || "untitled-page"}.md`;
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
    case "index":
      return "";
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

function splitEntries(value: string): string[] {
  return Array.from(
    new Set(
      value
        .split(/[\n,]/)
        .map((entry) => entry.trim())
        .filter(Boolean),
    ),
  );
}

function Field({
  label,
  hint,
  className,
  children,
}: {
  label: string;
  hint?: string;
  className?: string;
  children: ReactNode;
}) {
  return (
    <label className={className}>
      <span className="text-[11px] uppercase tracking-widest text-muted-foreground">
        {label}
      </span>
      <div className="mt-2">{children}</div>
      {hint ? <p className="mt-2 text-xs text-muted-foreground">{hint}</p> : null}
    </label>
  );
}
