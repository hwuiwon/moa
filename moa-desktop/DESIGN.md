# MOA Desktop Design System

This is the durable style guide for the MOA desktop app. Read it **before**
touching UI code so that panels stay consistent across sessions.

## Aesthetic

Minimal monochrome dev-tool UI — the pattern used by Linear, Superhuman,
Raycast, Cursor, and Conductor/Superconductor. The app feels like a
focused keyboard-driven tool, not a consumer product.

**Principles**

- **Monochrome base + single accent.** Near-black backgrounds, 4–5 grey
  steps, one blue primary. No secondary accents.
- **Density over decoration.** Information packs in. Icons communicate
  meaning; they don't fill space.
- **Typography carries hierarchy.** Size and weight differentiate levels,
  not colored backgrounds.
- **No emoji pictographs.** Use glyphs for directional/structural affordance
  only (`◀ ▶ ▾ ▸`). Avoid icon fonts in body content.
- **One accent, used deliberately.** Primary blue is for a single active
  state (selected nav row, toggle on, primary button). Don't wash the UI
  in blue.

## Tokens

All color/spacing references flow through `cx.theme().*`
(`gpui_component::ActiveTheme`). **No hard-coded hex values in panel code.**
The dark theme is authoritative — the settings Appearance tab can switch
to light, but every component must work in both.

| Use | Token |
|---|---|
| Window / chat background | `theme.background` |
| Elevated surface (cards, sidebar, titlebar, statusbar) | `theme.sidebar` |
| Muted surface (chips, inline code, track of a bar) | `theme.muted` |
| Accent surface (active nav row, selected list row) | `theme.accent` |
| Primary action (toggle on, primary button bg) | `theme.primary` |
| Body text | `theme.foreground` |
| Secondary / helper text | `theme.muted_foreground` |
| Primary-on-primary text | `theme.primary_foreground` |
| Accent-on-accent text | `theme.accent_foreground` |
| Borders, dividers | `theme.border` |
| Sidebar-interior borders, dividers | `theme.sidebar_border` |
| Destructive / error | `theme.danger` |
| Success | `theme.success` |
| Warning | `theme.warning` |

## Spacing

Use only `4 / 8 / 12 / 16 / 24` px (gpui: `gap_1 / gap_2 / gap_3 / gap_4 / gap_6`,
`p_*`, `px_*`, `py_*`). Anything in between is a smell.

| Situation | Value |
|---|---|
| Text row line height | 1.5–1.6 |
| Cluster gap inside a row | 8 px (`gap_2`) |
| Between sibling rows in a card | 12 px (`py_3` per row) |
| Card internal padding | 16 px (`p_4`) |
| Between cards in a section | 16 px (`gap_4`) |
| Between sections | 24 px (`gap_6`) |
| Toolbar / titlebar height | 30–36 px |

## Radius

- `rounded_sm` — chips, small inline badges.
- `rounded_md` — cards, modal panels, input fields, section containers.
- `rounded_full` — status dots, avatars, pill buttons, toggle thumbs.
- Never `rounded_lg` on anything that isn't a modal overlay.

## Typography

| Role | Size | Weight |
|---|---|---|
| Page title (e.g. "General") | `rems(1.25)` ≈ 20 px | semibold |
| Card title | `rems(0.9)` ≈ 14.5 px | medium |
| Row label | `text_sm` (14 px) | normal |
| Body / markdown | `text_sm` (14 px) | normal, line-height 1.55 |
| Muted description / metadata | `text_xs` (12 px) | normal |
| Inline chip label | `text_xs` (12 px) | normal |

System font stack (no custom font loading).

## Component patterns

All new panels should compose from the shared helpers in
`moa-desktop/src/components/*`. Don't inline-build these shapes — if a
pattern is missing from the helpers, add it there rather than duplicating.

### Icon button (`components::icon_button`)

Used in the titlebar and any toolbar.

- 28 × 28 px hit target, 14 px glyph.
- Default: `text_color(muted_foreground)`, no background.
- Hover: `bg(muted)`, `text_color(foreground)`.
- Active (toggle-on state): `bg(accent)`, `text_color(accent_foreground)`.

Anti-pattern: icon + text pairing in a toolbar. Keep it icon-only.

### Pill / primary button

- Use `gpui_component::button::Button::new(id).primary()` for destructive
  or high-signal positive actions ("Restart to update", "Test").
- `rounded_md`, `px_3 py_1p5`, `text_sm`.
- One primary button per card row at most.

### Toggle row (`components::settings_row` + `Switch`)

The canonical Settings row.

- Label on the left (14 px), muted description on the second line (12 px).
- `gpui_component::switch::Switch::new(id).checked(bool)` on the right.
- 12 px vertical padding, dividers auto-applied by `settings_row`.

### Section card (`components::section_card`)

Container for a group of rows.

- `bg(sidebar)`, `border_1 border_color(border)`, `rounded_md`, 16 px
  internal padding.
- Optional title + muted description above the first row.
- Rows stacked with 1-px `border_b` dividers (except last).

### Left-nav item (`components::nav_item`)

Used in the Settings page and anywhere else a left column of labels
selects content on the right.

- Text label only. No icon.
- Default: transparent bg, `text_color(sidebar_foreground)`.
- Hover: `bg(sidebar_accent)`.
- Active: `bg(primary)`, `text_color(primary_foreground)`.
- `rounded_md`, `px_3 py_1p5`.

### Dropdown

Use `gpui_component::select::Select`. Never hand-roll a menu of radio
buttons as a "dropdown substitute" — Select already handles the popover,
keyboard navigation, and focus.

### Status chip / inline badge

Small inline context: model name next to an assistant bubble, status
pill on a session row.

- `bg(muted)`, `text_color(muted_foreground)`, `rounded_sm`, `px_1p5 py_0p5`.
- `text_xs`. Never interactive — use a button if it needs to click.

## Layout rules

- **Titlebar**: icon-only. No text labels. Height 30–36 px.
- **Status bar**: numbers + dot on the right; state label on the left.
  Height 24–26 px.
- **Settings page**: `BackBar` (40 px) + `Body` (flex row). Left nav 240 px,
  right content `flex_1`, max-width 720 px for readability, scroll when
  exceeded.
- **Chat bubble**: left-aligned metadata row (model chip), main body,
  optional footer with tokens/cost. Markdown inside uses
  `components::markdown::markdown_style`.
- **Modal**: only for genuinely modal choices (command palette). Prefer a
  full-page view for multi-step flows (Settings).

## Density modes

The app supports two densities, selectable from **Settings → Appearance →
Density** and persisted to `~/.moa/config.toml` (`tui.density`).

- **Comfortable** (default): 12 px chat-bubble padding, 8 px row padding,
  1.55 markdown line-height.
- **Compact**: 8 px chat-bubble padding, 4 px row padding, 1.4 line-height.

Panel code reads the active density via `crate::density::current(cx)` and
applies the resulting `Spacing { bubble_padding, row_padding_y, list_gap,
markdown_line_height }` struct. **Never hard-code a padding value that
should flex with density** — if a surface's density affects readability,
route it through `density::current(cx).spacing()`.

## Semantic token layer (`theme_tokens.rs`)

New panel code should read colors through `theme_tokens::tokens(cx)`
which groups `cx.theme().*` into three buckets:

- `tokens(cx).base` — surface / foreground / border (monochrome scale).
- `tokens(cx).accent` — the single primary/accent family.
- `tokens(cx).contrast` — semantic notification colors (success,
  warning, danger, info).

The grouping mirrors Linear's three-variable theme. Existing code still
uses `cx.theme().*` directly; the token layer is additive. Migrate
files to `tokens(cx)` when you touch them — don't churn files purely
for the rename.

## WCAG contrast

Every foreground-on-background pairing introduced by a new panel must
pass **WCAG AA** (4.5:1 normal, 3:1 large). Helpers:

- `wcag::contrast_ratio(fg, bg) -> f32`
- `wcag::classify(fg, bg) -> WcagPass` (AaNormal / AaLargeOnly / Fail).

If a new semantic color fails AA on either theme, add an alt token
(e.g. `foreground_strong`) rather than tweaking the existing one —
preserving backward compatibility with code that intentionally wants
the softer value.

## Iconography

- Directional / structural glyphs OK: `◀ ▶ ▾ ▸ × ✕`.
- `⌘ ⇧ ⌥ ⏎ ⎋` keyboard symbols OK in shortcut hints.
- Plain dots `•` for timeline span markers — color conveys the type.
- No emoji pictographs (`💬 📝 📎 🤖`, etc.) anywhere in the UI.

## Do / Don't

**Do**
- Reach for `cx.theme().*` tokens.
- Compose from `components::*`.
- Preserve the 4/8/12/16/24 rhythm.
- Keep toolbars icon-only.
- Use `Select`/`Switch` from gpui-component instead of hand-rolled radios.

**Don't**
- Hard-code hex colors.
- Stack more than one accent in the same view.
- Add explanatory text to toolbar icons.
- Use `rounded_lg` outside modal overlays.
- Introduce a new emoji pictograph.
- Pick an arbitrary gap value (`gap_1p5`, `gap_5`, etc.).

## When this document is out of date

Update `design.md` **before** shipping the UI change that contradicts it.
The guide is only useful if it remains accurate.
