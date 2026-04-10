//! Two-pane memory browser with search and wikilink navigation.

use moa_core::{MemoryPath, MemorySearchResult, PageSummary, WikiPage};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};

/// Stateful memory-browser selection and history.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct MemoryViewState {
    query: String,
    selected: usize,
    search_mode: bool,
    pages: Vec<PageSummary>,
    search_results: Vec<MemorySearchResult>,
    current_page: Option<WikiPage>,
    history_back: Vec<MemoryPath>,
    history_forward: Vec<MemoryPath>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MemoryNavItem {
    Link(MemoryPath),
    Page(PageSummary),
    SearchResult(MemorySearchResult),
}

impl MemoryViewState {
    /// Creates a memory browser rooted in the provided page list.
    pub(crate) fn new(pages: Vec<PageSummary>) -> Self {
        Self {
            pages,
            ..Self::default()
        }
    }

    /// Returns the current search query.
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    /// Returns whether the view is currently editing the search query.
    pub(crate) fn search_mode(&self) -> bool {
        self.search_mode
    }

    /// Replaces the page listing while preserving navigation state.
    pub(crate) fn set_pages(&mut self, pages: Vec<PageSummary>) {
        self.pages = pages;
        self.clamp_selection();
    }

    /// Replaces the active search results.
    pub(crate) fn set_search_results(&mut self, results: Vec<MemorySearchResult>) {
        self.search_results = results;
        self.clamp_selection();
    }

    /// Replaces the currently opened page.
    pub(crate) fn set_current_page(&mut self, page: WikiPage) {
        self.current_page = Some(page);
    }

    /// Starts editing the search query.
    pub(crate) fn start_search(&mut self) {
        self.search_mode = true;
        self.selected = 0;
    }

    /// Stops editing the search query.
    pub(crate) fn stop_search(&mut self) {
        self.search_mode = false;
        self.selected = 0;
    }

    /// Appends a search character.
    pub(crate) fn push_query_char(&mut self, ch: char) {
        self.query.push(ch);
        self.selected = 0;
    }

    /// Removes one search character.
    pub(crate) fn backspace_query(&mut self) {
        self.query.pop();
        self.selected = 0;
    }

    /// Moves the selected row upward.
    pub(crate) fn move_up(&mut self) {
        let len = self.visible_items().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = if self.selected == 0 {
            len.saturating_sub(1)
        } else {
            self.selected.saturating_sub(1)
        };
    }

    /// Moves the selected row downward.
    pub(crate) fn move_down(&mut self) {
        let len = self.visible_items().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected + 1) % len;
    }

    /// Pushes a newly opened path onto the browser history.
    pub(crate) fn record_open(&mut self, path: &MemoryPath) {
        if self
            .current_page
            .as_ref()
            .and_then(|page| page.path.as_ref())
            .is_some_and(|current| current == path)
        {
            return;
        }
        if let Some(current) = self
            .current_page
            .as_ref()
            .and_then(|page| page.path.as_ref())
            .cloned()
        {
            self.history_back.push(current);
            self.history_forward.clear();
        }
    }

    /// Pops one page from browser history when possible.
    pub(crate) fn go_back(&mut self) -> Option<MemoryPath> {
        let current = self
            .current_page
            .as_ref()
            .and_then(|page| page.path.as_ref())
            .cloned()?;
        let previous = self.history_back.pop()?;
        self.history_forward.push(current);
        Some(previous)
    }

    /// Moves forward in browser history when possible.
    pub(crate) fn go_forward(&mut self) -> Option<MemoryPath> {
        let current = self
            .current_page
            .as_ref()
            .and_then(|page| page.path.as_ref())
            .cloned()?;
        let next = self.history_forward.pop()?;
        self.history_back.push(current);
        Some(next)
    }

    /// Returns the currently selected path to open.
    pub(crate) fn selected_path(&self) -> Option<MemoryPath> {
        self.visible_items()
            .get(self.selected)
            .map(MemoryNavItem::path)
    }

    /// Returns the number of visible navigation rows.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.visible_items().len()
    }

    fn visible_items(&self) -> Vec<MemoryNavItem> {
        if !self.query.trim().is_empty() {
            return self
                .search_results
                .iter()
                .cloned()
                .map(MemoryNavItem::SearchResult)
                .collect();
        }

        let mut items = Vec::new();
        for link in wikilinks_from_page(self.current_page.as_ref()) {
            items.push(MemoryNavItem::Link(link));
        }
        for page in &self.pages {
            items.push(MemoryNavItem::Page(page.clone()));
        }
        items
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_items().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }
}

impl MemoryNavItem {
    fn path(&self) -> MemoryPath {
        match self {
            Self::Link(path) => path.clone(),
            Self::Page(page) => page.path.clone(),
            Self::SearchResult(result) => result.path.clone(),
        }
    }

    fn render_label(&self) -> String {
        match self {
            Self::Link(path) => format!("↳ {}", path.as_str()),
            Self::Page(page) => format!("{} · {}", page.title, page.path.as_str()),
            Self::SearchResult(result) => {
                format!("{} · {}", result.title, result.path.as_str())
            }
        }
    }
}

/// Renders the full-screen memory browser.
pub(crate) fn render_memory_view(frame: &mut Frame<'_>, area: Rect, state: &MemoryViewState) {
    let popup = centered_rect(area, 92, 88);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(12),
            Constraint::Length(1),
        ])
        .split(popup);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
        .split(layout[1]);

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title("Memory Browser"),
        popup,
    );

    let query_title = if state.search_mode() {
        "Search · typing"
    } else {
        "Search"
    };
    let query = Paragraph::new(format!(
        "{}{}",
        if state.query().is_empty() { "/" } else { "" },
        state.query()
    ))
    .block(Block::default().borders(Borders::ALL).title(query_title));
    frame.render_widget(query, layout[0]);

    let mut nav_lines = Vec::new();
    if state.visible_items().is_empty() {
        nav_lines.push(Line::from("No memory pages."));
    } else {
        for (index, item) in state.visible_items().iter().enumerate().take(24) {
            let style = if index == state.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            nav_lines.push(Line::from(Span::styled(item.render_label(), style)));
            if let MemoryNavItem::SearchResult(result) = item {
                nav_lines.push(Line::from(Span::styled(
                    format!("  {}", result.snippet),
                    style,
                )));
            }
        }
    }
    frame.render_widget(
        Paragraph::new(nav_lines).block(Block::default().borders(Borders::ALL).title("Pages")),
        body[0],
    );

    let page_title = state
        .current_page
        .as_ref()
        .map(|page| page.title.clone())
        .unwrap_or_else(|| "No page selected".to_string());
    frame.render_widget(
        Paragraph::new(render_page_text(state.current_page.as_ref()))
            .block(Block::default().borders(Borders::ALL).title(page_title)),
        body[1],
    );

    let footer =
        Paragraph::new("/ search  Enter open  Alt+←/→ history  e editor  d delete  Esc close")
            .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, layout[2]);
}

fn render_page_text(page: Option<&WikiPage>) -> Text<'static> {
    let Some(page) = page else {
        return Text::from(vec![Line::from("Pick a page from the left pane.")]);
    };

    let mut lines = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        page.path
            .as_ref()
            .map(|path| path.as_str().to_string())
            .unwrap_or_else(|| "(virtual page)".to_string()),
        Style::default().add_modifier(Modifier::ITALIC),
    )]));
    lines.push(Line::raw(String::new()));
    for raw_line in page.content.lines() {
        if raw_line.starts_with('#') {
            lines.push(Line::from(vec![Span::styled(
                raw_line.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            )]));
        } else {
            lines.push(Line::raw(raw_line.to_string()));
        }
    }
    let links = wikilinks_from_page(Some(page));
    if !links.is_empty() {
        lines.push(Line::raw(String::new()));
        lines.push(Line::from(vec![Span::styled(
            "Wikilinks",
            Style::default().add_modifier(Modifier::BOLD),
        )]));
        for link in links {
            lines.push(Line::raw(format!("• {}", link.as_str())));
        }
    }

    Text::from(lines)
}

fn centered_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

/// Fuzzy-filters page summaries for external callers such as tests.
#[cfg(test)]
pub(crate) fn filter_pages<'a>(query: &str, pages: &'a [PageSummary]) -> Vec<&'a PageSummary> {
    use nucleo::{
        Config, Matcher, Utf32Str,
        pattern::{CaseMatching, Normalization, Pattern},
    };

    if query.trim().is_empty() {
        return pages.iter().collect();
    }

    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buffer = Vec::new();
    let mut scored = pages
        .iter()
        .filter_map(|page| {
            let haystack = format!("{} {}", page.title, page.path.as_str());
            let score = pattern.score(Utf32Str::new(&haystack, &mut buffer), &mut matcher)?;
            Some((page, score))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|(left_page, left_score), (right_page, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| right_page.updated.cmp(&left_page.updated))
    });
    scored.into_iter().map(|(page, _)| page).collect()
}

fn wikilinks_from_page(page: Option<&WikiPage>) -> Vec<MemoryPath> {
    let Some(page) = page else {
        return Vec::new();
    };

    let mut links = Vec::new();
    let mut remaining = page.content.as_str();
    while let Some(start) = remaining.find("[[") {
        remaining = &remaining[start + 2..];
        let Some(end) = remaining.find("]]") else {
            break;
        };
        let target = remaining[..end].trim();
        if !target.is_empty() {
            links.push(MemoryPath::new(target));
        }
        remaining = &remaining[end + 2..];
    }
    links
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{ConfidenceLevel, PageSummary, PageType};

    use super::{MemoryViewState, filter_pages};

    fn page(path: &str, title: &str) -> PageSummary {
        PageSummary {
            path: path.into(),
            title: title.to_string(),
            page_type: PageType::Topic,
            confidence: ConfidenceLevel::High,
            updated: Utc::now(),
        }
    }

    #[test]
    fn fuzzy_filter_matches_titles() {
        let pages = vec![
            page("auth/oauth.md", "OAuth Flow"),
            page("deploy/release.md", "Release Process"),
        ];

        let filtered = filter_pages("oauth", &pages);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title, "OAuth Flow");
    }

    #[test]
    fn memory_view_surfaces_wikilinks_before_page_list() {
        let mut state = MemoryViewState::new(vec![page("auth/oauth.md", "OAuth Flow")]);
        state.set_current_page(moa_core::WikiPage {
            path: Some("notes.md".into()),
            title: "Notes".to_string(),
            page_type: PageType::Topic,
            content: "See [[auth/oauth.md]] for details.".to_string(),
            created: Utc::now(),
            updated: Utc::now(),
            confidence: ConfidenceLevel::Medium,
            related: Vec::new(),
            sources: Vec::new(),
            tags: Vec::new(),
            auto_generated: false,
            last_referenced: Utc::now(),
            reference_count: 0,
            metadata: std::collections::HashMap::new(),
        });

        assert_eq!(
            state.selected_path().map(|path| path.to_string()),
            Some("auth/oauth.md".to_string())
        );
        assert!(state.len() >= 2);
    }
}
