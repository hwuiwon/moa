import { useQuery } from "@tanstack/react-query";
import { invoke } from "@tauri-apps/api/core";
import { RefreshCw } from "lucide-react";

import { Button } from "@/components/ui/button";

type SessionSummary = {
  sessionId: string;
  workspaceId: string;
  userId: string;
  title: string | null;
  status: string;
  platform: string;
  model: string;
  updatedAt: string;
  active: boolean;
};

function formatTimestamp(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return value;
  }

  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(date);
}

function App() {
  const sessions = useQuery({
    queryKey: ["sessions"],
    queryFn: () => invoke<SessionSummary[]>("list_sessions"),
  });

  return (
    <main className="min-h-screen bg-[radial-gradient(circle_at_top,#d9ebff,transparent_40%),linear-gradient(180deg,#f8fbff_0%,#eef4fb_100%)] text-slate-950">
      <div className="mx-auto flex min-h-screen max-w-5xl flex-col px-6 py-10">
        <header className="mb-10 flex items-center justify-between">
          <div>
            <p className="text-sm font-medium uppercase tracking-[0.2em] text-slate-500">
              Desktop Runtime
            </p>
            <h1 className="mt-2 text-4xl font-semibold tracking-tight">MOA</h1>
            <p className="mt-3 max-w-2xl text-sm text-slate-600">
              Tauri v2 backend scaffold wired to the real chat runtime. This page
              proves the Rust IPC boundary by listing sessions from the managed
              runtime state.
            </p>
          </div>
          <Button
            onClick={() => void sessions.refetch()}
            type="button"
          >
            <RefreshCw className="h-4 w-4" />
            Refresh
          </Button>
        </header>

        <section className="rounded-3xl border border-white/60 bg-white/80 p-6 shadow-[0_20px_80px_-40px_rgba(15,23,42,0.45)] backdrop-blur">
          <div className="mb-4 flex items-center justify-between">
            <div>
              <h2 className="text-lg font-semibold">Sessions</h2>
              <p className="text-sm text-slate-500">
                Command: <code>invoke("list_sessions")</code>
              </p>
            </div>
            <span className="rounded-full bg-slate-100 px-3 py-1 text-xs font-medium text-slate-600">
              {sessions.data?.length ?? 0} loaded
            </span>
          </div>

          {sessions.isLoading ? (
            <p className="text-sm text-slate-500">Loading sessions…</p>
          ) : sessions.isError ? (
            <pre className="overflow-x-auto rounded-2xl bg-slate-950 px-4 py-3 text-sm text-rose-200">
              {String(sessions.error)}
            </pre>
          ) : sessions.data && sessions.data.length > 0 ? (
            <div className="overflow-hidden rounded-2xl border border-slate-200">
              <table className="min-w-full divide-y divide-slate-200 text-left text-sm">
                <thead className="bg-slate-50 text-slate-500">
                  <tr>
                    <th className="px-4 py-3 font-medium">Session</th>
                    <th className="px-4 py-3 font-medium">Workspace</th>
                    <th className="px-4 py-3 font-medium">Model</th>
                    <th className="px-4 py-3 font-medium">Updated</th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-slate-200 bg-white">
                  {sessions.data.map((session) => (
                    <tr key={session.sessionId}>
                      <td className="px-4 py-3">
                        <div className="font-medium text-slate-900">
                          {session.title ?? "Untitled session"}
                        </div>
                        <div className="mt-1 text-xs text-slate-500">
                          {session.sessionId}
                          {session.active ? " • active" : ""}
                        </div>
                      </td>
                      <td className="px-4 py-3 text-slate-600">
                        {session.workspaceId}
                      </td>
                      <td className="px-4 py-3 text-slate-600">{session.model}</td>
                      <td className="px-4 py-3 text-slate-600">
                        {formatTimestamp(session.updatedAt)}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ) : (
            <div className="rounded-2xl border border-dashed border-slate-300 bg-slate-50 px-4 py-8 text-center text-sm text-slate-500">
              No sessions yet. The managed runtime initialized successfully, but
              there are no persisted sessions for the current workspace.
            </div>
          )}
        </section>
      </div>
    </main>
  );
}

export default App;
