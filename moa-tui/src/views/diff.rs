//! Full-screen diff viewer and diff-rendering helpers for approvals.

use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};
use similar::{ChangeTag, TextDiff};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Style as SyntectStyle, Theme, ThemeSet},
    parsing::{SyntaxReference, SyntaxSet},
};

use crate::runner::ApprovalFileDiff;

const SIDE_BY_SIDE_WIDTH: u16 = 120;
const DIFF_CONTEXT_LINES: usize = 3;

/// Full-screen diff layout mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffMode {
    /// One unified diff column.
    Unified,
    /// Old and new versions rendered side-by-side.
    SideBySide,
}

/// Stateful diff viewer data derived from one or more approval file diffs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffViewState {
    files: Vec<DiffFileView>,
    current_file: usize,
    current_hunk: usize,
    mode: DiffMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffFileView {
    path: String,
    language_hint: Option<String>,
    unified_lines: Vec<UnifiedLine>,
    split_lines: Vec<SplitLine>,
    hunk_ranges: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UnifiedLine {
    old_lineno: Option<usize>,
    new_lineno: Option<usize>,
    tag: ChangeTag,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SplitLine {
    old_lineno: Option<usize>,
    old_text: String,
    new_lineno: Option<usize>,
    new_text: String,
    tag: ChangeTag,
}

impl DiffViewState {
    /// Creates a diff viewer state from the pending approval file diffs.
    pub(crate) fn from_file_diffs(file_diffs: &[ApprovalFileDiff], width: u16) -> Option<Self> {
        if file_diffs.is_empty() {
            return None;
        }

        let files = file_diffs.iter().map(build_diff_file_view).collect();
        Some(Self {
            files,
            current_file: 0,
            current_hunk: 0,
            mode: default_mode_for_width(width),
        })
    }

    /// Toggles between unified and side-by-side rendering when the terminal is wide enough.
    pub(crate) fn toggle_mode(&mut self, width: u16) {
        self.mode = match (self.mode, width >= SIDE_BY_SIDE_WIDTH) {
            (DiffMode::Unified, true) => DiffMode::SideBySide,
            _ => DiffMode::Unified,
        };
    }

    /// Advances to the next diff file.
    pub(crate) fn next_file(&mut self) {
        if self.files.is_empty() {
            return;
        }
        self.current_file = (self.current_file + 1) % self.files.len();
        self.current_hunk = 0;
    }

    /// Moves to the previous diff file.
    pub(crate) fn previous_file(&mut self) {
        if self.files.is_empty() {
            return;
        }
        self.current_file = if self.current_file == 0 {
            self.files.len().saturating_sub(1)
        } else {
            self.current_file.saturating_sub(1)
        };
        self.current_hunk = 0;
    }

    /// Advances to the next diff hunk within the current file.
    pub(crate) fn next_hunk(&mut self) {
        let Some(file) = self.current_file() else {
            return;
        };
        if file.hunk_ranges.is_empty() {
            return;
        }
        self.current_hunk = (self.current_hunk + 1) % file.hunk_ranges.len();
    }

    /// Moves to the previous diff hunk within the current file.
    pub(crate) fn previous_hunk(&mut self) {
        let Some(file) = self.current_file() else {
            return;
        };
        if file.hunk_ranges.is_empty() {
            return;
        }
        self.current_hunk = if self.current_hunk == 0 {
            file.hunk_ranges.len().saturating_sub(1)
        } else {
            self.current_hunk.saturating_sub(1)
        };
    }

    fn current_file(&self) -> Option<&DiffFileView> {
        self.files.get(self.current_file)
    }

    fn current_hunk_range(&self) -> Option<&Range<usize>> {
        self.current_file()
            .and_then(|file| file.hunk_ranges.get(self.current_hunk))
    }
}

/// Returns the default diff mode for the current terminal width.
pub(crate) fn default_mode_for_width(width: u16) -> DiffMode {
    if width >= SIDE_BY_SIDE_WIDTH {
        DiffMode::SideBySide
    } else {
        DiffMode::Unified
    }
}

/// Renders the full-screen diff viewer overlay.
pub(crate) fn render_diff_view(frame: &mut Frame<'_>, area: Rect, state: &DiffViewState) {
    frame.render_widget(Clear, area);

    let Some(file) = state.current_file() else {
        let empty = Paragraph::new("No diff available.")
            .block(Block::default().borders(Borders::ALL).title("Diff"));
        frame.render_widget(empty, area);
        return;
    };

    let selected_hunk = state.current_hunk_range().cloned().unwrap_or(0..0);
    let title = format!(
        "Diff · {} · file {}/{} · hunk {}/{} · {} · t toggle · n/N file · j/k hunk · a accept · r reject · Esc close",
        file.path,
        state.current_file + 1,
        state.files.len(),
        state.current_hunk + 1,
        file.hunk_ranges.len().max(1),
        match state.mode {
            DiffMode::Unified => "unified",
            DiffMode::SideBySide => "side-by-side",
        }
    );

    let lines = match state.mode {
        DiffMode::Unified => render_unified_lines(file, selected_hunk.clone(), area.width),
        DiffMode::SideBySide => render_side_by_side_lines(file, selected_hunk.clone(), area.width),
    };
    let scroll = selected_hunk.start.saturating_sub(2) as u16;
    let paragraph = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

/// Renders a compact unified diff preview used by the inline approval widget.
pub(crate) fn render_compact_diff_preview(
    diff: &ApprovalFileDiff,
    width: u16,
    max_lines: usize,
) -> Vec<Line<'static>> {
    let file = build_diff_file_view(diff);
    let Some(first_hunk) = file.hunk_ranges.first() else {
        return vec![Line::raw("No textual changes detected.")];
    };

    let line_width = width.saturating_sub(2);
    let unified = render_unified_lines(&file, first_hunk.clone(), line_width);
    let mut preview = unified.into_iter().take(max_lines).collect::<Vec<_>>();
    if first_hunk.len() > max_lines {
        preview.push(Line::raw(format!(
            "  … {} more diff lines",
            first_hunk.len().saturating_sub(max_lines)
        )));
    }
    preview
}

fn build_diff_file_view(diff: &ApprovalFileDiff) -> DiffFileView {
    let text_diff = TextDiff::from_lines(&diff.before, &diff.after);
    let grouped = text_diff.grouped_ops(DIFF_CONTEXT_LINES);

    let mut unified_lines = Vec::new();
    let mut split_lines = Vec::new();
    let mut hunk_ranges = Vec::new();
    let mut old_lineno = 1usize;
    let mut new_lineno = 1usize;

    if grouped.is_empty() {
        unified_lines.push(UnifiedLine {
            old_lineno: Some(1),
            new_lineno: Some(1),
            tag: ChangeTag::Equal,
            text: diff.after.clone(),
        });
        split_lines.push(SplitLine {
            old_lineno: Some(1),
            old_text: diff.before.clone(),
            new_lineno: Some(1),
            new_text: diff.after.clone(),
            tag: ChangeTag::Equal,
        });
        hunk_ranges.push(0..1);
    } else {
        for group in grouped {
            let start = unified_lines.len();
            for op in group {
                for change in text_diff.iter_changes(&op) {
                    let text = trim_change_line(change.to_string());
                    match change.tag() {
                        ChangeTag::Delete => {
                            unified_lines.push(UnifiedLine {
                                old_lineno: Some(old_lineno),
                                new_lineno: None,
                                tag: ChangeTag::Delete,
                                text: text.clone(),
                            });
                            split_lines.push(SplitLine {
                                old_lineno: Some(old_lineno),
                                old_text: text,
                                new_lineno: None,
                                new_text: String::new(),
                                tag: ChangeTag::Delete,
                            });
                            old_lineno += 1;
                        }
                        ChangeTag::Insert => {
                            unified_lines.push(UnifiedLine {
                                old_lineno: None,
                                new_lineno: Some(new_lineno),
                                tag: ChangeTag::Insert,
                                text: text.clone(),
                            });
                            split_lines.push(SplitLine {
                                old_lineno: None,
                                old_text: String::new(),
                                new_lineno: Some(new_lineno),
                                new_text: text,
                                tag: ChangeTag::Insert,
                            });
                            new_lineno += 1;
                        }
                        ChangeTag::Equal => {
                            unified_lines.push(UnifiedLine {
                                old_lineno: Some(old_lineno),
                                new_lineno: Some(new_lineno),
                                tag: ChangeTag::Equal,
                                text: text.clone(),
                            });
                            split_lines.push(SplitLine {
                                old_lineno: Some(old_lineno),
                                old_text: text.clone(),
                                new_lineno: Some(new_lineno),
                                new_text: text,
                                tag: ChangeTag::Equal,
                            });
                            old_lineno += 1;
                            new_lineno += 1;
                        }
                    }
                }
            }
            hunk_ranges.push(start..unified_lines.len());
        }
    }

    DiffFileView {
        path: diff.path.clone(),
        language_hint: diff.language_hint.clone(),
        unified_lines,
        split_lines,
        hunk_ranges,
    }
}

fn render_unified_lines(
    file: &DiffFileView,
    selected_hunk: Range<usize>,
    width: u16,
) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(2).max(24) as usize;
    file.unified_lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let highlight = selected_hunk.contains(&index);
            render_unified_line(
                line,
                file.language_hint.as_deref(),
                content_width,
                highlight,
            )
        })
        .collect()
}

fn render_unified_line(
    line: &UnifiedLine,
    language_hint: Option<&str>,
    width: usize,
    highlight: bool,
) -> Line<'static> {
    let base = diff_line_style(line.tag, highlight);
    let marker = match line.tag {
        ChangeTag::Delete => '-',
        ChangeTag::Insert => '+',
        ChangeTag::Equal => ' ',
    };
    let prefix = format!(
        "{:>4} {:>4} {marker} ",
        line.old_lineno
            .map(|value| value.to_string())
            .unwrap_or_default(),
        line.new_lineno
            .map(|value| value.to_string())
            .unwrap_or_default(),
    );
    let reserved = prefix.chars().count();
    let text_width = width.saturating_sub(reserved);
    let text = truncate_to_width(&line.text, text_width);

    let mut spans = vec![Span::styled(prefix, base)];
    spans.extend(highlighted_spans(&text, language_hint, base));
    Line::from(spans)
}

fn render_side_by_side_lines(
    file: &DiffFileView,
    selected_hunk: Range<usize>,
    width: u16,
) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(2).max(40) as usize;
    let column_width = content_width.saturating_sub(3) / 2;
    file.split_lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let highlight = selected_hunk.contains(&index);
            render_split_line(line, file.language_hint.as_deref(), column_width, highlight)
        })
        .collect()
}

fn render_split_line(
    line: &SplitLine,
    language_hint: Option<&str>,
    column_width: usize,
    highlight: bool,
) -> Line<'static> {
    let left_base = split_side_style(true, line.tag, highlight);
    let right_base = split_side_style(false, line.tag, highlight);
    let left = render_side(
        line.old_lineno,
        &line.old_text,
        side_marker(true, line.tag),
        column_width,
        language_hint,
        left_base,
    );
    let right = render_side(
        line.new_lineno,
        &line.new_text,
        side_marker(false, line.tag),
        column_width,
        language_hint,
        right_base,
    );

    let mut spans = left;
    spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
    spans.extend(right);
    Line::from(spans)
}

fn render_side(
    lineno: Option<usize>,
    text: &str,
    marker: char,
    width: usize,
    language_hint: Option<&str>,
    base: Style,
) -> Vec<Span<'static>> {
    let prefix = format!(
        "{} {:>4} ",
        marker,
        lineno.map(|value| value.to_string()).unwrap_or_default()
    );
    let reserved = prefix.chars().count();
    let text_width = width.saturating_sub(reserved);
    let mut spans = vec![Span::styled(prefix, base)];
    spans.extend(highlighted_spans(
        &pad_or_truncate(text, text_width),
        language_hint,
        base,
    ));
    spans
}

fn side_marker(left: bool, tag: ChangeTag) -> char {
    match (left, tag) {
        (_, ChangeTag::Equal) => ' ',
        (true, ChangeTag::Delete) => '-',
        (false, ChangeTag::Insert) => '+',
        _ => ' ',
    }
}

fn diff_line_style(tag: ChangeTag, highlight: bool) -> Style {
    let base = match tag {
        ChangeTag::Delete => Style::default().fg(Color::LightRed),
        ChangeTag::Insert => Style::default().fg(Color::LightGreen),
        ChangeTag::Equal => Style::default().fg(Color::White),
    };
    if highlight {
        base.bg(Color::DarkGray)
    } else {
        base
    }
}

fn split_side_style(left: bool, tag: ChangeTag, highlight: bool) -> Style {
    let base = match (left, tag) {
        (true, ChangeTag::Delete) => Style::default().fg(Color::LightRed),
        (false, ChangeTag::Insert) => Style::default().fg(Color::LightGreen),
        _ => Style::default().fg(Color::White),
    };
    if highlight {
        base.bg(Color::DarkGray)
    } else {
        base
    }
}

fn highlighted_spans(text: &str, language_hint: Option<&str>, base: Style) -> Vec<Span<'static>> {
    let Some(theme) = theme() else {
        return vec![Span::styled(text.to_string(), base)];
    };

    let syntax = syntax_for_hint(language_hint);
    let mut highlighter = HighlightLines::new(syntax, theme);
    match highlighter.highlight_line(text, syntax_set()) {
        Ok(regions) => regions
            .into_iter()
            .map(|(style, segment)| {
                Span::styled(segment.to_string(), base.patch(to_ratatui_style(style)))
            })
            .collect(),
        Err(_) => vec![Span::styled(text.to_string(), base)],
    }
}

fn syntax_for_hint(language_hint: Option<&str>) -> &'static SyntaxReference {
    let syntax_set = syntax_set();
    language_hint
        .and_then(|hint| {
            Path::new(hint)
                .extension()
                .and_then(|extension| extension.to_str())
                .or(Some(hint))
        })
        .and_then(|extension| syntax_set.find_syntax_by_extension(extension))
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text())
}

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> Option<&'static Theme> {
    static THEME: OnceLock<Option<Theme>> = OnceLock::new();
    THEME
        .get_or_init(|| {
            let themes = ThemeSet::load_defaults();
            themes
                .themes
                .get("base16-ocean.dark")
                .cloned()
                .or_else(|| themes.themes.values().next().cloned())
        })
        .as_ref()
}

fn to_ratatui_style(style: SyntectStyle) -> Style {
    let mut mapped = Style::default()
        .fg(Color::Rgb(
            style.foreground.r,
            style.foreground.g,
            style.foreground.b,
        ))
        .bg(Color::Rgb(
            style.background.r,
            style.background.g,
            style.background.b,
        ));
    if style.font_style.contains(FontStyle::BOLD) {
        mapped = mapped.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        mapped = mapped.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        mapped = mapped.add_modifier(Modifier::UNDERLINED);
    }
    mapped
}

fn trim_change_line(text: String) -> String {
    text.trim_end_matches('\n').to_string()
}

fn truncate_to_width(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

fn pad_or_truncate(text: &str, width: usize) -> String {
    let truncated = truncate_to_width(text, width);
    let pad = width.saturating_sub(truncated.chars().count());
    format!("{truncated}{}", " ".repeat(pad))
}

#[cfg(test)]
mod tests {
    use super::{DiffMode, default_mode_for_width};

    #[test]
    fn diff_layout_switches_at_expected_threshold() {
        assert_eq!(default_mode_for_width(80), DiffMode::Unified);
        assert_eq!(default_mode_for_width(119), DiffMode::Unified);
        assert_eq!(default_mode_for_width(120), DiffMode::SideBySide);
        assert_eq!(default_mode_for_width(180), DiffMode::SideBySide);
    }
}
