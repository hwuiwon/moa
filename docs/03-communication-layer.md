# 03 — Communication Layer

_Messaging gateway, TUI, CLI, approval UX, thread observation._

---

## Messaging gateway

### Architecture

Single Rust binary with per-platform tokio task isolation. Feature flags control which adapters compile in.

```toml
# Cargo.toml features
[features]
default = ["tui"]
telegram = ["teloxide"]
slack = ["slack-morphism"]
discord = ["serenity"]
cloud = ["telegram", "slack", "discord"]
```

### Message normalization

All platforms normalize to a common inbound format:

```rust
pub struct InboundMessage {
    pub platform: Platform,
    pub platform_msg_id: String,
    pub user: PlatformUser,
    pub channel: ChannelRef,       // DM, group, thread
    pub text: String,
    pub attachments: Vec<Attachment>,
    pub reply_to: Option<String>,  // if replying to a specific message
    pub timestamp: DateTime<Utc>,
}

pub struct PlatformUser {
    pub platform_id: String,
    pub display_name: String,
    pub moa_user_id: Option<UserId>, // linked MOA user, if known
}

pub enum ChannelRef {
    DirectMessage { user_id: String },
    Group { channel_id: String },
    Thread { channel_id: String, thread_id: String },
}
```

### Outbound rendering

Outbound messages are rendered per-platform from a common format:

```rust
pub struct OutboundMessage {
    pub content: MessageContent,
    pub buttons: Vec<ActionButton>,
    pub reply_to: Option<String>,
    pub ephemeral: bool,           // only visible to one user (Slack/Discord)
}

pub enum MessageContent {
    Text(String),
    Markdown(String),
    CodeBlock { language: String, code: String },
    Diff { filename: String, hunks: Vec<DiffHunk> },
    ToolCard { tool: String, status: ToolStatus, summary: String, detail: Option<String> },
    ApprovalRequest { request: ApprovalRequest },
    StatusUpdate { session_id: SessionId, status: SessionStatus, summary: String },
}

pub struct ActionButton {
    pub id: String,
    pub label: String,
    pub style: ButtonStyle,  // Primary, Danger, Secondary
    pub callback_data: String,
}
```

The `PlatformRenderer` trait converts `OutboundMessage` to platform-native format:

```rust
pub trait PlatformRenderer {
    fn render_text(&self, md: &str) -> String;           // markdown → platform format
    fn render_code(&self, lang: &str, code: &str) -> String;
    fn render_diff(&self, diff: &[DiffHunk]) -> Vec<String>; // may split across messages
    fn render_buttons(&self, buttons: &[ActionButton]) -> PlatformButtons;
    fn truncate(&self, text: &str) -> (String, bool);    // respects platform char limit
}
```

### Platform-specific limits and rendering

| Feature | Telegram | Slack | Discord |
|---|---|---|---|
| Max message | 4,096 chars | 40,000 chars (50 blocks) | 2,000 chars (+ 4,096 embed) |
| Code blocks | ` ```lang ` | ` ```lang ` | ` ```diff ` in embeds |
| Buttons | InlineKeyboardMarkup (callback 64 bytes) | Block Kit actions (primary/danger) | ActionRow (5 button styles) |
| Edit window | 48 hours | Unlimited | Unlimited |
| Threads | Reply chains | Native threads | Auto-created threads |
| Modals | Web App (Mini App) | Native modals | Native modals |
| Rate limit | 30 msg/sec | 1 msg/sec per channel | 5 req/sec per channel |
| Status update interval | 2-3s (editMessageText) | 1-2s (chat.update) | 2-5s (message edit) |

### Session ↔ platform mapping

Each MOA session maps to a platform thread/conversation:

- **Telegram**: Each session = a message thread (reply chain). Status message pinned at top, updated in-place.
- **Slack**: Each session = a thread. Parent message = live status. Replies = event log. App Home = multi-session dashboard.
- **Discord**: Each session = an auto-created thread. Embed = status display. ActionRow = control buttons.

---

## Approval UX

### Three-tier buttons

Every tool call requiring approval renders:

```
┌─ 🟡 bash ──────────────────────────────────────┐
│ Command: npm install express                     │
│ Working dir: ~/projects/webapp                   │
│                                                  │
│ [✅ Allow Once]  [🔁 Always Allow]  [❌ Deny]  │
└──────────────────────────────────────────────────┘
```

Risk-level coloring:
- 🟢 Green: read-only operations (file_read, web_search)
- 🟡 Yellow: file modifications (file_write, file_create)
- 🔴 Red: shell commands, network access, destructive operations

### "Always Allow" rule storage

Rules stored per-workspace in `~/.moa/workspaces/{id}/permissions.toml`:

```toml
[[rules]]
tool = "file_read"
pattern = "**"           # glob pattern for arguments
scope = "workspace"      # workspace | session | global
created_by = "user123"
created_at = "2026-04-09T14:30:00Z"

[[rules]]
tool = "bash"
pattern = "npm test*"    # only allow npm test commands
scope = "workspace"

[[rules]]
tool = "bash"
pattern = "rm *"
action = "deny"          # always deny rm commands
```

Rules match at the **inner command level** — a `bash` approval for `npm test` does not approve `npm test && rm -rf /`. The shell command is parsed before matching.

### Post-decision rendering

After the user decides, edit the original message in-place:

```
┌─ ✅ bash ── Allowed by @user at 14:30 ─────────┐
│ Command: npm install express                     │
│ Working dir: ~/projects/webapp                   │
│ Result: added 52 packages in 3.2s               │
└──────────────────────────────────────────────────┘
```

---

## Thread observation

### Launch scope: Observe + Stop + Queue

**Observe**: Subscribe to session event stream.

Three detail levels:
- **Summary**: SessionStatus changes, Checkpoint summaries, Errors
- **Normal**: + ToolCall names and results, BrainResponse text
- **Verbose**: + streaming tokens, full tool parameters, thinking summaries

Platform rendering of observations:

```
Telegram: Single status message, edited every 2-3s
  "🔄 Working on OAuth fix...
   ✅ file_search: found 3 files
   🔧 file_read: src/auth/refresh.rs
   💭 Analyzing token expiry logic...
   [⏹ Stop] [📋 Queue Message]"

Slack: Thread with live-updating parent
  Parent: "🔄 Session: OAuth fix | 4 tools used | 12.3k tokens"
  Reply 1: "✅ file_search → 3 files found"
  Reply 2: "🔧 Reading src/auth/refresh.rs"
  ...

Discord: Thread with embed
  Embed: status, tool count, token count
  Messages: one per significant event
```

**Stop**: Sends `CancelRequested` signal.
- Soft stop: completes current tool call, then stops
- Hard stop (long-press or double-tap): aborts immediately

**Queue**: User sends a message while a run is active.
- Platform detects the session is running
- Message stored durably in the session log and queued for the next turn
- Brain picks it up after current turn completes
- User sees: "📋 Message queued. Will process after current task."

---

## TUI specification

### Layout (5 zones)

```
┌─[1 oauth-fix 🔄][2 deploy ✅][3 research ⏸]─────────────────────┐
│ MOA  workspace: webapp  model: claude-sonnet  12.3k/200k  $0.12   │ ← Header
├─────────────────────────────────────────────┬──────────────────────┤
│                                             │ Session Info         │
│  User: Fix the OAuth refresh token bug      │ ──────────          │
│                                             │ Duration: 4m 23s    │
│  Agent: I'll investigate the auth module... │ Turns: 7            │
│                                             │ Tools: 3 calls      │
│  ┌─ 🔧 file_search ────── ✅ Done ──┐     │ Cost: $0.12         │
│  │ Pattern: "oauth.*token"            │     │                     │
│  │ Found: 3 files                     │     │ Workspace Tools     │ ← Sidebar
│  └────────────────────────────────────┘     │ ──────────          │ (auto at 120+ cols)
│                                             │ ✓ bash              │
│  The issue is in `auth/refresh.rs`...       │ ✓ file_read         │
│                                             │ ✓ file_write        │
│  ┌─ 🟡 file_write ─── ⏳ Approval ──┐     │ ✓ web_search        │
│  │ Path: src/auth/refresh.rs          │     │                     │
│  │ +12 -3 lines                       │     │ Memory              │
│  │                                    │     │ ──────────          │
│  │ [Y]es [N]o [A]lways [D]iff [E]dit │     │ • Auth system       │ ← Chat stream
│  └────────────────────────────────────┘     │ • Deploy guide      │
│                                             │ • API conventions    │
├─────────────────────────────────────────────┴──────────────────────┤
│ > @auth/refresh.rs Fix the token expiry logic█                      │ ← Prompt
├─────────────────────────────────────────────────────────────────────┤
│ approve: y/n/a │ ctrl+x h: help │ /cmd │ @file │ !shell   cost:$0 │ ← Footer
└─────────────────────────────────────────────────────────────────────┘
```

### Zones

1. **Tab bar**: Session tabs with status icons. `Alt+1-9` direct switch. `Alt+[/]` cycle. `Ctrl+N` new. Max 8 visible, overflow to picker.
2. **Header**: Workspace name, model, token usage (current/max), cumulative cost, mode indicator.
3. **Chat stream**: Scrollable message history with inline tool cards, approval widgets, and diff previews. Auto-scrolls during streaming. Pauses on user scroll-up. Resumes with `End`.
4. **Sidebar** (optional): Session info (duration, turns, tools, cost), workspace tools list, recent memory entries. Auto-shows at 120+ cols. Toggle with `Ctrl+X, B`.
5. **Prompt**: Rich text input via `tui-textarea`. Supports `@filename` completion (frecency-ranked), `/command` completion, `!shell` prefix, `Shift+Enter` for newline, `Enter` to submit.
6. **Footer**: Context-sensitive shortcuts (changes based on active view), mode indicator, cost ticker.

### Views (modes)

| View | Shortcut | Description |
|---|---|---|
| Chat | (default) | Main conversation view |
| Sessions | `Ctrl+O, S` | Fuzzy-searchable session picker overlay |
| Memory | `Ctrl+M` | Two-pane wiki browser (tree + rendered markdown) |
| Settings | `Ctrl+,` | Config editor (categories + form widgets) |
| Diff | `D` (on approval) | Full-screen diff viewer with side-by-side / unified toggle |
| Help | `Ctrl+X, H` | Keybinding reference |

### Keyboard shortcuts

**Universal (all views):**

| Shortcut | Action |
|---|---|
| `Ctrl+C` | Cancel current operation / interrupt |
| `Ctrl+D` | Exit (on empty input) |
| `Ctrl+L` | Clear screen |
| `Escape` | Cancel streaming / close overlay / back |
| `Alt+1-9` | Switch to session tab N |
| `Alt+[` / `Alt+]` | Cycle session tabs |
| `Ctrl+N` | New session |
| `Ctrl+P` | Command palette (fuzzy search all actions) |
| `Ctrl+M` | Memory browser |
| `Ctrl+,` | Settings |
| `Ctrl+X, H` | Help |
| `Ctrl+X, B` | Toggle sidebar |

**Chat view:**

| Shortcut | Action |
|---|---|
| `Enter` | Submit message |
| `Shift+Enter` | Insert newline |
| `Up` / `Down` | Input history |
| `Ctrl+R` | Search input history |
| `Ctrl+G` | Open in external editor ($EDITOR) |
| `Tab` | Autocomplete (@file, /command) |
| `Ctrl+U` / `Ctrl+D` | Scroll chat up/down (half page) |
| `Home` / `End` | Jump to top/bottom of chat |
| `v` | Cycle observation verbosity |

**Approval prompt (when focused):**

| Shortcut | Action |
|---|---|
| `y` | Allow once |
| `n` | Deny |
| `a` | Always allow |
| `d` | Show full diff |
| `e` | Edit parameters |
| `Shift+A` | Batch approve all pending |

**Diff view:**

| Shortcut | Action |
|---|---|
| `t` | Toggle side-by-side / unified |
| `n` / `N` | Next / previous file |
| `j` / `k` | Next / previous hunk |
| `+` / `-` | Expand / collapse context lines |
| `a` | Accept changes (per-file or per-hunk) |
| `r` | Reject changes |
| `f` / `Escape` | Toggle full screen |

**Memory browser:**

| Shortcut | Action |
|---|---|
| `/` | Search memories |
| `Enter` | Open selected page / follow wiki link |
| `Alt+←` / `Alt+→` | Back / forward (browser-style) |
| `e` | Edit selected page in $EDITOR |
| `d` | Delete selected page (with confirmation) |

### Prompt features

- **`@filename`**: Tab-complete file paths. Frecency-ranked (recently used files rank higher). Files are added to context for the current message.
- **`/command`**: Slash commands. Tab-complete from registered list.
- **`!command`**: Shell escape. Run command directly, output shown inline.
- **Multiline**: `Shift+Enter` inserts newline. Input expands up to 10 lines, then scrolls.
- **Paste detection**: Multi-line paste auto-wraps in code block.
- **Image paste**: Detect image in clipboard, attach as base64 for vision models.

### Slash commands

```
/new              Start a new session
/sessions         Open session picker
/resume [id]      Resume a specific or most recent session
/model [name]     Switch model (or show picker)
/memory           Open memory browser
/workspace [path] Switch workspace
/tools            Show/configure available tools
/settings         Open settings
/compact          Force context compaction
/export [format]  Export session (markdown, json)
/undo             Revert last file change
/redo             Redo last reverted change
/clear            Clear chat display (doesn't delete history)
/editor           Open current context in $EDITOR
/status           Show session stats (tokens, cost, duration)
/help             Show help
/quit             Exit
```

---

## CLI specification

### Subcommands

```
moa                          Interactive TUI (default)
moa "prompt text"            TUI with pre-submitted prompt
moa exec "prompt text"       Non-interactive one-shot
moa exec --json "prompt"     JSON output for scripting
moa status                   Show active sessions (table)
moa sessions                 List all sessions
moa sessions --workspace .   Sessions for current workspace
moa attach <session-id>      Attach TUI to running session
moa resume                   Resume most recent session in TUI
moa resume <session-id>      Resume specific session
moa memory search "query"    Search memory from CLI
moa memory show <path>       Display a memory page
moa config                   Show current config
moa config set <key> <val>   Update config
moa init                     Initialize workspace in current directory
moa version                  Show version info
moa doctor                   Check system health (Docker, API keys, etc.)
```

### Non-interactive mode (`moa exec`)

```bash
# Basic one-shot
moa exec "What's the weather in NYC?"

# Pipe input
cat error.log | moa exec "Explain these errors"

# JSON output for scripting
moa exec --json "List all TODO items in the codebase" | jq '.items'

# Specify model
moa exec --model gpt-4o "Translate this to Japanese"

# Specify workspace
moa exec --workspace ~/projects/webapp "Run the tests"
```

Behavior:
- Progress → stderr (streaming status updates)
- Final response → stdout
- Exit code: 0 = success, 1 = error, 2 = user cancelled
- `--json` outputs JSONL (one event per line) to stdout
- `--bare` outputs raw text only (no formatting, no ANSI)
- TTY detection: if stdin is not a terminal, read piped input as context

### Daemon mode (`moa daemon`)

For local persistent background operation:

```bash
moa daemon start             Start background daemon
moa daemon stop              Stop daemon
moa daemon status            Show daemon status
moa daemon logs              Tail daemon logs
```

The daemon runs the `LocalOrchestrator` in the background, allowing sessions to continue when the TUI is closed. The TUI connects to the daemon over a local Unix socket.

---

## Ratatui crate stack

| Purpose | Crate | Notes |
|---|---|---|
| TUI framework | `ratatui` | Core rendering |
| Terminal backend | `crossterm` | Cross-platform terminal control |
| Async runtime | `tokio` | Async I/O, task spawning |
| Text input | `tui-textarea` | Vim-like editing, used by rainfrog |
| Overlays | `tui-overlay` | Drawers, modals, toasts |
| Fuzzy matching | `nucleo` | Same matcher as Helix editor |
| Markdown rendering | `pulldown-cmark` + `syntect` | Parse + syntax highlight |
| Diff rendering | `similar` | Diff algorithm |
| Form widgets | `rat-widget` | Settings panel inputs |
| CLI parsing | `clap` | Derive-based argument parsing |
