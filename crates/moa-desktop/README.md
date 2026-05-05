# moa-desktop

Native desktop app for MOA — a GPUI-based Rust binary that embeds the full
MOA runtime (sessions, memory, skills, providers) behind a three-panel
workspace.

## Prerequisites

### Rust

Stable toolchain, edition 2024. Install via [rustup.rs](https://rustup.rs/)
if you don't have it already.

### Platform libraries

| Platform | Requirement |
|---|---|
| macOS | **Full Xcode.app** (GPUI's build calls `xcrun metal`; Command Line Tools alone fails) |
| Linux | `libxkbcommon-dev`, `libwayland-dev`, a working Vulkan driver (used by the wgpu backend) |
| Windows | Microsoft Visual C++ Build Tools (MSVC toolchain) |

### Configuration

MOA reads its config from `~/.moa/config.toml`. On first run the app
falls back to defaults; the settings panel (⌘,) writes changes to that
path as you interact. Provider API keys are sourced from environment
variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GOOGLE_API_KEY`) —
the settings panel surfaces their status but never stores secrets.

## Build

From the workspace root:

```bash
# Compile only, fast feedback loop:
cargo check -p moa-desktop

# Debug build:
cargo build -p moa-desktop

# Optimized release build:
cargo build -p moa-desktop --release
```

## Run

```bash
cargo run -p moa-desktop
```

The app opens at your previously-saved size/position (stored in
`~/.moa/window_state.json`) and appears both as a window and as a
system-tray icon.

## Keyboard shortcuts

### Global

| Shortcut | Action |
|---|---|
| ⌘K | Command palette (fuzzy-searchable action list) |
| ⌘, | Settings (General / Providers / Permissions) |
| ⌘N | New session |
| ⌘W | Close session |
| ⌘\\ | Toggle sidebar |
| ⇧⌘\\ | Toggle detail panel |
| ⌘L | Focus prompt composer |
| ⇧⌘L | Focus sidebar |
| ⌘M | Open memory browser |
| ⇧⌘K | Open skill manager |
| ⌘] | Next session |
| ⌘[ | Previous session |
| ⌘. | Stop current session |
| ⌘R | Refresh memory |
| ⇧⌘M | Search memory |
| Esc | Dismiss modal |
| ⌘Q | Quit (fully exits, including tray) |

### Contextual

| Shortcut | Context | Action |
|---|---|---|
| Y | Approval card focused | Approve once |
| A | Approval card focused | Always allow |
| N | Approval card focused | Deny |
| ↑ / ↓ | Command palette | Navigate results |
| Enter | Command palette | Confirm selection |

## Close-to-tray

Closing the window hides the app to the system tray rather than
quitting (first time: a toast reminds you). Click the tray icon's
**Show MOA** to bring it back. **Quit MOA** in the tray menu, or ⌘Q,
fully exits the process.

## Drag-and-drop

Drop files onto the chat panel to attach them. File paths appear as
chips above the composer and are prepended to the prompt on submit as
`[attachment: /path/to/file]`. Remove a chip with its `×` button
before sending if you change your mind.

## Testing

```bash
# Unit tests + integration tests:
cargo test -p moa-desktop

# Clippy (treat warnings as errors):
cargo clippy -p moa-desktop -- -D warnings
```

## Troubleshooting

- **macOS build fails with "unable to find utility `metal`"** — install
  full Xcode.app, not just Command Line Tools, then
  `sudo xcode-select -s /Applications/Xcode.app/Contents/Developer`.
- **Window opens off-screen** — delete `~/.moa/window_state.json` to
  reset to centered defaults.
- **Tray icon missing on Linux** — install a status-notifier-item
  provider (`libappindicator3`); the app logs a warning and continues
  without a tray if none is available.
