//! Fuzzy command palette overlay (Cmd+K).
//!
//! Rendered as a centered modal above the workspace. Holds a list of
//! [`CommandEntry`] values and filters them by fuzzy subsequence match on
//! each render. Selection is driven by the `PaletteMoveUp`/`Down`/`Confirm`
//! actions scoped to the `"CommandPalette"` key context.

use gpui::{
    Action, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyContext, MouseButton, ParentElement, Render, SharedString, Styled, Window, div, hsla,
    prelude::*, px,
};
use gpui_component::{
    ActiveTheme,
    input::{Input, InputEvent, InputState},
};

use crate::actions::{
    DismissModal, FocusPrompt, NewSession, OpenMemoryBrowser, OpenSettings, OpenSkillManager,
    PaletteConfirm, PaletteMoveDown, PaletteMoveUp, ToggleDetailPanel, ToggleSidebar,
};

/// Event emitted when the palette should be closed (Escape, outside click, or
/// after confirming a selection).
#[derive(Clone, Debug)]
pub struct PaletteDismissed;

/// One row in the palette.
pub struct CommandEntry {
    pub name: SharedString,
    pub description: Option<SharedString>,
    pub shortcut: Option<SharedString>,
    pub action: Box<dyn Action>,
}

impl CommandEntry {
    fn new(
        name: impl Into<SharedString>,
        description: Option<&'static str>,
        shortcut: Option<&'static str>,
        action: impl Action,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.map(SharedString::from),
            shortcut: shortcut.map(SharedString::from),
            action: Box::new(action),
        }
    }
}

fn default_commands() -> Vec<CommandEntry> {
    vec![
        CommandEntry::new(
            "New Session",
            Some("Start a fresh conversation"),
            Some("⌘N"),
            NewSession,
        ),
        CommandEntry::new(
            "Open Settings",
            Some("Configure MOA"),
            Some("⌘,"),
            OpenSettings,
        ),
        CommandEntry::new(
            "Toggle Sidebar",
            Some("Show or hide the session sidebar"),
            Some("⌘\\"),
            ToggleSidebar,
        ),
        CommandEntry::new(
            "Toggle Detail Panel",
            Some("Show or hide the timeline panel"),
            Some("⇧⌘\\"),
            ToggleDetailPanel,
        ),
        CommandEntry::new(
            "Focus Prompt",
            Some("Move focus to the message composer"),
            Some("⌘L"),
            FocusPrompt,
        ),
        CommandEntry::new(
            "Open Memory Browser",
            Some("Browse workspace memory"),
            Some("⌘M"),
            OpenMemoryBrowser,
        ),
        CommandEntry::new(
            "Open Skill Manager",
            Some("Browse and manage skills"),
            Some("⇧⌘K"),
            OpenSkillManager,
        ),
    ]
}

/// The palette view itself.
pub struct CommandPalette {
    query_input: Entity<InputState>,
    commands: Vec<CommandEntry>,
    matches: Vec<(usize, i32)>, // (command index, fuzzy score) — sorted desc by score
    selected: usize,
    focus: FocusHandle,
    history: PaletteHistory,
}

/// Most-recent-first ordering of command names the user has confirmed.
/// Persisted between launches so the palette always surfaces the commands
/// each individual user actually reaches for (Raycast / VSCode pattern).
#[derive(Default, Clone, Debug)]
struct PaletteHistory {
    recent: Vec<String>,
}

impl PaletteHistory {
    const CAP: usize = 20;

    fn default_path() -> Option<std::path::PathBuf> {
        std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .map(|home| home.join(".moa").join("palette_history.json"))
    }

    fn load_or_default() -> Self {
        let Some(path) = Self::default_path() else {
            return Self::default();
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        let recent: Vec<String> = serde_json::from_slice(&bytes).unwrap_or_default();
        Self { recent }
    }

    fn save(&self) {
        let Some(path) = Self::default_path() else {
            return;
        };
        if let Some(parent) = path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(%err, "palette history: mkdir parent failed");
            return;
        }
        let Ok(bytes) = serde_json::to_vec(&self.recent) else {
            return;
        };
        if let Err(err) = std::fs::write(&path, bytes) {
            tracing::warn!(%err, "palette history: write failed");
        }
    }

    /// Inserts `name` at the head, drops duplicates elsewhere, caps the
    /// list at `CAP` entries. Called on every successful confirm.
    fn bump(&mut self, name: &str) {
        self.recent.retain(|n| n != name);
        self.recent.insert(0, name.to_string());
        self.recent.truncate(Self::CAP);
    }

    /// Position of `name` in history; `None` means never confirmed.
    /// Lower index = more recent.
    fn rank(&self, name: &str) -> Option<usize> {
        self.recent.iter().position(|n| n == name)
    }
}

impl EventEmitter<PaletteDismissed> for CommandPalette {}

impl Focusable for CommandPalette {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl CommandPalette {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query_input = cx.new(|cx| InputState::new(window, cx).placeholder("Type a command…"));

        // Rerun the filter on every keystroke by re-rendering.
        cx.subscribe(&query_input, |this, _, _event: &InputEvent, cx| {
            this.refilter(cx);
            cx.notify();
        })
        .detach();

        let focus = cx.focus_handle();
        let commands = default_commands();
        let history = PaletteHistory::load_or_default();
        let matches = initial_ordering(&commands, &history);
        Self {
            query_input,
            commands,
            matches,
            selected: 0,
            focus,
            history,
        }
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.query_input.read(cx).text().to_string();
        if query.is_empty() {
            // Empty query: surface recently-used commands on top.
            self.matches = initial_ordering(&self.commands, &self.history);
        } else {
            let mut scored: Vec<(usize, i32)> = self
                .commands
                .iter()
                .enumerate()
                .filter_map(|(i, cmd)| fuzzy_score(&query, &cmd.name).map(|s| (i, s)))
                .collect();
            // Tie-break equal fuzzy scores with recency so common matches
            // the user tends to pick float to the top of the list.
            scored.sort_by(|a, b| {
                b.1.cmp(&a.1).then_with(|| {
                    let a_rank = self
                        .history
                        .rank(&self.commands[a.0].name)
                        .unwrap_or(usize::MAX);
                    let b_rank = self
                        .history
                        .rank(&self.commands[b.0].name)
                        .unwrap_or(usize::MAX);
                    a_rank.cmp(&b_rank)
                })
            });
            self.matches = scored;
        }
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    fn move_up(&mut self, _: &PaletteMoveUp, _window: &mut Window, cx: &mut Context<Self>) {
        if self.selected > 0 {
            self.selected -= 1;
            cx.notify();
        }
    }

    fn move_down(&mut self, _: &PaletteMoveDown, _window: &mut Window, cx: &mut Context<Self>) {
        if self.selected + 1 < self.matches.len() {
            self.selected += 1;
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &PaletteConfirm, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_selected(window, cx);
    }

    fn dismiss(&mut self, _: &DismissModal, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PaletteDismissed);
    }

    /// Closes the palette first, then dispatches the chosen action on the
    /// window so the root view has already dropped the overlay by the time
    /// focus flows to whichever view handles the action.
    fn dispatch_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (action, name) = match self.matches.get(self.selected).copied() {
            Some((idx, _)) => {
                let entry = self.commands.get(idx);
                (
                    entry.map(|e| e.action.boxed_clone()),
                    entry.map(|e| e.name.to_string()),
                )
            }
            None => (None, None),
        };
        if let Some(name) = name {
            self.history.bump(&name);
            self.history.save();
        }
        cx.emit(PaletteDismissed);
        if let Some(action) = action {
            window.dispatch_action(action, cx);
        }
    }
}

/// Builds the default (empty-query) match ordering: history-ranked
/// commands first (in most-recent-first order), then any remaining
/// commands in their originally-defined order.
fn initial_ordering(commands: &[CommandEntry], history: &PaletteHistory) -> Vec<(usize, i32)> {
    let mut ranked: Vec<(usize, i32)> = commands
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let rank = history.rank(&cmd.name).unwrap_or(usize::MAX);
            // Encode rank inversely as a descending score so the sort
            // produces history entries first; unranked entries share
            // the lowest score and keep their original relative order.
            let score = if rank == usize::MAX {
                -(i as i32)
            } else {
                i32::MAX - rank as i32
            };
            (i, score)
        })
        .collect();
    ranked.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
    ranked
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let matches = self.matches.clone();
        let selected = self.selected;

        // List of rows.
        let mut list = div()
            .id("palette-list")
            .flex()
            .flex_col()
            .max_h(px(320.))
            .overflow_y_scroll();
        if matches.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_4()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No matches"),
            );
        } else {
            for (row_idx, (cmd_idx, _score)) in matches.iter().enumerate() {
                let Some(entry) = self.commands.get(*cmd_idx) else {
                    continue;
                };
                let is_selected = row_idx == selected;
                let row_bg = if is_selected {
                    theme.accent
                } else {
                    theme.background
                };
                let row_fg = if is_selected {
                    theme.accent_foreground
                } else {
                    theme.foreground
                };
                let desc = entry.description.clone();
                let shortcut = entry.shortcut.clone();
                let name = entry.name.clone();
                let row_id: SharedString = format!("palette-row-{row_idx}").into();
                let stored_idx = row_idx;
                list = list.child(
                    div()
                        .id(gpui::ElementId::Name(row_id))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .px_3()
                        .py_2()
                        .bg(row_bg)
                        .text_color(row_fg)
                        .hover(|s| s.bg(theme.accent))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.selected = stored_idx;
                                this.dispatch_selected(window, cx);
                            }),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .child(div().text_sm().child(name))
                                .when_some(desc, |d, description| {
                                    d.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(description),
                                    )
                                }),
                        )
                        .when_some(shortcut, |d, sc| {
                            d.child(div().text_xs().text_color(theme.muted_foreground).child(sc))
                        }),
                );
            }
        }

        let input_bar = div()
            .p_2()
            .border_b_1()
            .border_color(theme.border)
            .child(Input::new(&self.query_input).cleanable(false));

        let key_context = {
            let mut kc = KeyContext::new_with_defaults();
            kc.add("CommandPalette");
            kc
        };

        // Backdrop: captures clicks outside the panel.
        div()
            .key_context(key_context)
            .track_focus(&self.focus)
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(120.))
            .bg(hsla(0., 0., 0., 0.45))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.emit(PaletteDismissed)),
            )
            .child(
                // Panel: stops click propagation so clicks inside don't dismiss.
                div()
                    .id("palette-panel")
                    .w(px(560.))
                    .max_h(px(440.))
                    .flex()
                    .flex_col()
                    .bg(theme.background)
                    .border_1()
                    .border_color(theme.border)
                    .rounded_lg()
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(input_bar)
                    .child(list),
            )
    }
}

// -- Fuzzy matching ---------------------------------------------------------

/// Returns `Some(score)` if all query characters appear as a subsequence of
/// `target` (case-insensitive). Higher scores are better.
///
/// Scoring heuristics (matches the spec's reference):
/// - +1 per character match
/// - +2 per consecutive run character (growing)
/// - +5 bonus for matching the first character of the target
fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    let query = query.to_lowercase();
    let target = target.to_lowercase();
    let mut score = 0i32;
    let mut query_iter = query.chars().peekable();
    let mut consecutive = 0i32;

    for (i, c) in target.chars().enumerate() {
        let Some(&qc) = query_iter.peek() else {
            break;
        };
        if c == qc {
            score += 1 + consecutive * 2;
            if i == 0 {
                score += 5;
            }
            consecutive += 1;
            query_iter.next();
        } else {
            consecutive = 0;
        }
    }

    if query_iter.peek().is_none() {
        Some(score)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::fuzzy_score;

    #[test]
    fn matches_subsequence() {
        assert!(fuzzy_score("ns", "New Session").is_some());
        assert!(fuzzy_score("set", "Open Settings").is_some());
        assert!(fuzzy_score("xyz", "New Session").is_none());
    }

    #[test]
    fn rewards_prefix() {
        let a = fuzzy_score("new", "New Session").unwrap();
        let b = fuzzy_score("new", "Thing Named New").unwrap();
        assert!(a > b, "prefix match should score higher: {a} vs {b}");
    }

    #[test]
    fn rewards_consecutive() {
        let a = fuzzy_score("sess", "Session").unwrap();
        let b = fuzzy_score("sess", "s_e_s_s").unwrap();
        assert!(a > b, "consecutive run should score higher: {a} vs {b}");
    }
}
