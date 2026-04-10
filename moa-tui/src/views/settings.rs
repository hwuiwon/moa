//! Settings panel state and rendering helpers.

use moa_core::MoaConfig;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

/// Selection state for the TUI settings panel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SettingsViewState {
    category: usize,
    field: usize,
}

/// Outcome of mutating one setting entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SettingsMutation {
    /// No further runtime action required.
    None,
    /// The runtime provider/model should be reloaded.
    ModelChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsCategory {
    General,
    Tui,
    Permissions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsField {
    DefaultProvider,
    DefaultModel,
    ReasoningEffort,
    SidebarAuto,
    TabLimit,
    DiffStyle,
    DefaultPosture,
}

impl SettingsViewState {
    /// Creates a new settings-view state.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Moves the selected field upward.
    pub(crate) fn move_up(&mut self, config: &MoaConfig) {
        let len = fields_for(self.category(config)).len();
        if len == 0 {
            self.field = 0;
            return;
        }
        self.field = if self.field == 0 {
            len - 1
        } else {
            self.field.saturating_sub(1)
        };
    }

    /// Moves the selected field downward.
    pub(crate) fn move_down(&mut self, config: &MoaConfig) {
        let len = fields_for(self.category(config)).len();
        if len == 0 {
            self.field = 0;
            return;
        }
        self.field = (self.field + 1) % len;
    }

    /// Moves to the previous settings category.
    pub(crate) fn move_left(&mut self) {
        self.category = self.category.saturating_sub(1);
        self.field = 0;
    }

    /// Moves to the next settings category.
    pub(crate) fn move_right(&mut self) {
        self.category = (self.category + 1).min(SETTINGS_CATEGORIES.len().saturating_sub(1));
        self.field = 0;
    }

    /// Applies one forward step to the selected setting.
    pub(crate) fn step_forward(&self, config: &mut MoaConfig) -> SettingsMutation {
        mutate_setting(config, self.current_field(config), true)
    }

    /// Applies one backward step to the selected setting.
    pub(crate) fn step_backward(&self, config: &mut MoaConfig) -> SettingsMutation {
        mutate_setting(config, self.current_field(config), false)
    }

    fn category(&self, _config: &MoaConfig) -> SettingsCategory {
        SETTINGS_CATEGORIES[self.category]
    }

    fn current_field(&self, config: &MoaConfig) -> SettingsField {
        let fields = fields_for(self.category(config));
        fields[self.field.min(fields.len().saturating_sub(1))]
    }
}

/// Renders the settings panel overlay.
pub(crate) fn render_settings_view(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &SettingsViewState,
    config: &MoaConfig,
) {
    let popup = centered_rect(area, 82, 82);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(12), Constraint::Length(1)])
        .split(popup);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(20), Constraint::Min(20)])
        .split(layout[0]);

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default().borders(Borders::ALL).title("Settings"),
        popup,
    );

    let category = state.category(config);
    let category_lines = SETTINGS_CATEGORIES
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let style = if index == state.category {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(category_label(*item), style))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(category_lines)
            .block(Block::default().borders(Borders::ALL).title("Categories")),
        body[0],
    );

    let fields = fields_for(category)
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let style = if index == state.field {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(Span::styled(
                format!("{}: {}", field_label(*field), field_value(config, *field)),
                style,
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(fields).block(Block::default().borders(Borders::ALL).title("Values")),
        body[1],
    );

    frame.render_widget(
        Paragraph::new("←/→ category  ↑/↓ field  Enter/→ cycle  Esc close")
            .block(Block::default().borders(Borders::ALL)),
        layout[1],
    );
}

const SETTINGS_CATEGORIES: [SettingsCategory; 3] = [
    SettingsCategory::General,
    SettingsCategory::Tui,
    SettingsCategory::Permissions,
];

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

fn fields_for(category: SettingsCategory) -> &'static [SettingsField] {
    match category {
        SettingsCategory::General => &[
            SettingsField::DefaultProvider,
            SettingsField::DefaultModel,
            SettingsField::ReasoningEffort,
        ],
        SettingsCategory::Tui => &[
            SettingsField::SidebarAuto,
            SettingsField::TabLimit,
            SettingsField::DiffStyle,
        ],
        SettingsCategory::Permissions => &[SettingsField::DefaultPosture],
    }
}

fn category_label(category: SettingsCategory) -> &'static str {
    match category {
        SettingsCategory::General => "General",
        SettingsCategory::Tui => "TUI",
        SettingsCategory::Permissions => "Permissions",
    }
}

fn field_label(field: SettingsField) -> &'static str {
    match field {
        SettingsField::DefaultProvider => "Default Provider",
        SettingsField::DefaultModel => "Default Model",
        SettingsField::ReasoningEffort => "Reasoning",
        SettingsField::SidebarAuto => "Sidebar Auto",
        SettingsField::TabLimit => "Tab Limit",
        SettingsField::DiffStyle => "Diff Style",
        SettingsField::DefaultPosture => "Default Posture",
    }
}

fn field_value(config: &MoaConfig, field: SettingsField) -> String {
    match field {
        SettingsField::DefaultProvider => config.general.default_provider.clone(),
        SettingsField::DefaultModel => config.general.default_model.clone(),
        SettingsField::ReasoningEffort => config.general.reasoning_effort.clone(),
        SettingsField::SidebarAuto => config.tui.sidebar_auto.to_string(),
        SettingsField::TabLimit => config.tui.tab_limit.to_string(),
        SettingsField::DiffStyle => config.tui.diff_style.clone(),
        SettingsField::DefaultPosture => config.permissions.default_posture.clone(),
    }
}

fn mutate_setting(config: &mut MoaConfig, field: SettingsField, forward: bool) -> SettingsMutation {
    match field {
        SettingsField::DefaultProvider => {
            cycle_string(
                &mut config.general.default_provider,
                &["openai", "anthropic", "openrouter"],
                forward,
            );
            config.general.default_model = provider_default_model(&config.general.default_provider);
            SettingsMutation::ModelChanged
        }
        SettingsField::DefaultModel => {
            let options = model_options(&config.general.default_provider);
            cycle_string(&mut config.general.default_model, options, forward);
            SettingsMutation::ModelChanged
        }
        SettingsField::ReasoningEffort => {
            cycle_string(
                &mut config.general.reasoning_effort,
                &["low", "medium", "high"],
                forward,
            );
            SettingsMutation::None
        }
        SettingsField::SidebarAuto => {
            config.tui.sidebar_auto = !config.tui.sidebar_auto;
            SettingsMutation::None
        }
        SettingsField::TabLimit => {
            let mut value = config.tui.tab_limit as isize;
            value += if forward { 4 } else { -4 };
            config.tui.tab_limit = value.clamp(4, 16) as usize;
            SettingsMutation::None
        }
        SettingsField::DiffStyle => {
            cycle_string(
                &mut config.tui.diff_style,
                &["auto", "unified", "split"],
                forward,
            );
            SettingsMutation::None
        }
        SettingsField::DefaultPosture => {
            cycle_string(
                &mut config.permissions.default_posture,
                &["approve", "review", "deny"],
                forward,
            );
            SettingsMutation::None
        }
    }
}

fn provider_default_model(provider: &str) -> String {
    model_options(provider)
        .first()
        .copied()
        .unwrap_or("gpt-5.4")
        .to_string()
}

fn model_options(provider: &str) -> &'static [&'static str] {
    match provider {
        "anthropic" => &["claude-sonnet-4-6", "claude-opus-4-6"],
        "openrouter" => &["openai/gpt-5.4", "anthropic/claude-sonnet-4-6"],
        _ => &["gpt-5.4", "gpt-5.4-mini", "gpt-4.1"],
    }
}

fn cycle_string(current: &mut String, options: &[&str], forward: bool) {
    let current_index = options
        .iter()
        .position(|value| *value == current)
        .unwrap_or(0);
    let next_index = if forward {
        (current_index + 1) % options.len()
    } else if current_index == 0 {
        options.len().saturating_sub(1)
    } else {
        current_index.saturating_sub(1)
    };
    *current = options[next_index].to_string();
}

#[cfg(test)]
mod tests {
    use moa_core::MoaConfig;

    use super::{SettingsMutation, SettingsViewState};

    #[test]
    fn cycling_provider_updates_default_model() {
        let mut config = MoaConfig::default();
        let state = SettingsViewState::new();

        assert_eq!(
            state.step_forward(&mut config),
            SettingsMutation::ModelChanged
        );
        assert_eq!(config.general.default_provider, "anthropic");
        assert_eq!(config.general.default_model, "claude-sonnet-4-6");
    }
}
