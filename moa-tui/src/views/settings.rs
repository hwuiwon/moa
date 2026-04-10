//! Settings panel state and rendering helpers.

use moa_core::{DatabaseBackend, MoaConfig};
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
    editor: Option<SettingsEditorState>,
}

/// Outcome of mutating one setting entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SettingsMutation {
    /// Whether the provider/model runtime should be reloaded immediately.
    pub(crate) reload_model_path: bool,
    /// Optional status note to surface in the chat transcript.
    pub(crate) notice: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SettingsEditorState {
    draft: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsCategory {
    General,
    Tui,
    Local,
    Database,
    Permissions,
    Daemon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsField {
    DefaultProvider,
    DefaultModel,
    ReasoningEffort,
    WorkspaceInstructions,
    UserInstructions,
    Theme,
    SidebarAuto,
    TabLimit,
    DiffStyle,
    DockerEnabled,
    SandboxDir,
    MemoryDir,
    DatabaseBackend,
    DatabaseUrl,
    DatabaseAdminUrl,
    DatabasePoolMin,
    DatabasePoolMax,
    DatabaseConnectTimeout,
    DefaultPosture,
    AutoApproveCount,
    AlwaysDenyCount,
    DaemonAutoConnect,
}

impl SettingsViewState {
    /// Creates a new settings-view state.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns whether the panel is editing a freeform value.
    pub(crate) fn is_editing(&self) -> bool {
        self.editor.is_some()
    }

    /// Starts editing the selected field when it supports freeform input.
    pub(crate) fn begin_edit(&mut self, config: &MoaConfig) -> bool {
        let field = self.current_field(config);
        let Some(draft) = editable_value(config, field) else {
            return false;
        };
        self.editor = Some(SettingsEditorState { draft });
        true
    }

    /// Appends one character to the current draft value.
    pub(crate) fn push_edit_char(&mut self, ch: char) {
        if let Some(editor) = &mut self.editor {
            editor.draft.push(ch);
        }
    }

    /// Removes one character from the current draft value.
    pub(crate) fn backspace_edit(&mut self) {
        if let Some(editor) = &mut self.editor {
            editor.draft.pop();
        }
    }

    /// Cancels the current freeform edit session.
    pub(crate) fn cancel_edit(&mut self) {
        self.editor = None;
    }

    /// Commits the current draft into the selected field.
    pub(crate) fn commit_edit(&mut self, config: &mut MoaConfig) -> SettingsMutation {
        let Some(editor) = self.editor.take() else {
            return SettingsMutation::default();
        };
        apply_text_edit(config, self.current_field(config), editor.draft)
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
        self.editor = None;
    }

    /// Moves to the next settings category.
    pub(crate) fn move_right(&mut self) {
        self.category = (self.category + 1).min(SETTINGS_CATEGORIES.len().saturating_sub(1));
        self.field = 0;
        self.editor = None;
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

    fn draft(&self) -> Option<&str> {
        self.editor.as_ref().map(|editor| editor.draft.as_str())
    }
}

/// Renders the settings panel overlay.
pub(crate) fn render_settings_view(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &SettingsViewState,
    config: &MoaConfig,
) {
    let popup = centered_rect(area, 88, 84);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(12), Constraint::Length(3)])
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
            let selected = index == state.field;
            let mut value = field_value(config, *field);
            if selected
                && state.is_editing()
                && let Some(draft) = state.draft()
            {
                value = format!("{draft}_");
            }
            let label = if field_needs_follow_up(*field) {
                format!("{} *", field_label(*field))
            } else {
                field_label(*field).to_string()
            };
            let style = if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(Span::styled(format!("{label}: {value}"), style))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(fields).block(Block::default().borders(Borders::ALL).title("Values")),
        body[1],
    );

    let selected_field = state.current_field(config);
    let footer = if state.is_editing() {
        "Enter save  Esc cancel  Backspace delete  Type to edit"
    } else {
        field_footer(selected_field)
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(footer),
            Line::raw(field_detail(selected_field)),
        ])
        .block(Block::default().borders(Borders::ALL)),
        layout[1],
    );
}

const SETTINGS_CATEGORIES: [SettingsCategory; 6] = [
    SettingsCategory::General,
    SettingsCategory::Tui,
    SettingsCategory::Local,
    SettingsCategory::Database,
    SettingsCategory::Permissions,
    SettingsCategory::Daemon,
];

const GENERAL_FIELDS: [SettingsField; 5] = [
    SettingsField::DefaultProvider,
    SettingsField::DefaultModel,
    SettingsField::ReasoningEffort,
    SettingsField::WorkspaceInstructions,
    SettingsField::UserInstructions,
];

const TUI_FIELDS: [SettingsField; 4] = [
    SettingsField::Theme,
    SettingsField::SidebarAuto,
    SettingsField::TabLimit,
    SettingsField::DiffStyle,
];

const LOCAL_FIELDS: [SettingsField; 3] = [
    SettingsField::DockerEnabled,
    SettingsField::SandboxDir,
    SettingsField::MemoryDir,
];

const DATABASE_FIELDS: [SettingsField; 6] = [
    SettingsField::DatabaseBackend,
    SettingsField::DatabaseUrl,
    SettingsField::DatabaseAdminUrl,
    SettingsField::DatabasePoolMin,
    SettingsField::DatabasePoolMax,
    SettingsField::DatabaseConnectTimeout,
];

const PERMISSION_FIELDS: [SettingsField; 3] = [
    SettingsField::DefaultPosture,
    SettingsField::AutoApproveCount,
    SettingsField::AlwaysDenyCount,
];

const DAEMON_FIELDS: [SettingsField; 1] = [SettingsField::DaemonAutoConnect];

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
        SettingsCategory::General => &GENERAL_FIELDS,
        SettingsCategory::Tui => &TUI_FIELDS,
        SettingsCategory::Local => &LOCAL_FIELDS,
        SettingsCategory::Database => &DATABASE_FIELDS,
        SettingsCategory::Permissions => &PERMISSION_FIELDS,
        SettingsCategory::Daemon => &DAEMON_FIELDS,
    }
}

fn category_label(category: SettingsCategory) -> &'static str {
    match category {
        SettingsCategory::General => "General",
        SettingsCategory::Tui => "TUI",
        SettingsCategory::Local => "Local",
        SettingsCategory::Database => "Database",
        SettingsCategory::Permissions => "Permissions",
        SettingsCategory::Daemon => "Daemon",
    }
}

fn field_label(field: SettingsField) -> &'static str {
    match field {
        SettingsField::DefaultProvider => "Default Provider",
        SettingsField::DefaultModel => "Default Model",
        SettingsField::ReasoningEffort => "Reasoning",
        SettingsField::WorkspaceInstructions => "Workspace Instr.",
        SettingsField::UserInstructions => "User Instr.",
        SettingsField::Theme => "Theme",
        SettingsField::SidebarAuto => "Sidebar Auto",
        SettingsField::TabLimit => "Tab Limit",
        SettingsField::DiffStyle => "Diff Style",
        SettingsField::DockerEnabled => "Docker Enabled",
        SettingsField::SandboxDir => "Sandbox Dir",
        SettingsField::MemoryDir => "Memory Dir",
        SettingsField::DatabaseBackend => "Backend",
        SettingsField::DatabaseUrl => "Runtime URL",
        SettingsField::DatabaseAdminUrl => "Admin URL",
        SettingsField::DatabasePoolMin => "Pool Min",
        SettingsField::DatabasePoolMax => "Pool Max",
        SettingsField::DatabaseConnectTimeout => "Connect Timeout",
        SettingsField::DefaultPosture => "Default Posture",
        SettingsField::AutoApproveCount => "Auto-Approve",
        SettingsField::AlwaysDenyCount => "Always-Deny",
        SettingsField::DaemonAutoConnect => "Auto Connect",
    }
}

fn field_value(config: &MoaConfig, field: SettingsField) -> String {
    match field {
        SettingsField::DefaultProvider => config.general.default_provider.clone(),
        SettingsField::DefaultModel => config.general.default_model.clone(),
        SettingsField::ReasoningEffort => config.general.reasoning_effort.clone(),
        SettingsField::WorkspaceInstructions => {
            summarize_optional_text(config.general.workspace_instructions.as_deref())
        }
        SettingsField::UserInstructions => {
            summarize_optional_text(config.general.user_instructions.as_deref())
        }
        SettingsField::Theme => config.tui.theme.clone(),
        SettingsField::SidebarAuto => bool_label(config.tui.sidebar_auto),
        SettingsField::TabLimit => config.tui.tab_limit.to_string(),
        SettingsField::DiffStyle => config.tui.diff_style.clone(),
        SettingsField::DockerEnabled => bool_label(config.local.docker_enabled),
        SettingsField::SandboxDir => config.local.sandbox_dir.clone(),
        SettingsField::MemoryDir => config.local.memory_dir.clone(),
        SettingsField::DatabaseBackend => config.database.backend.as_str().to_string(),
        SettingsField::DatabaseUrl => config.database.url.clone(),
        SettingsField::DatabaseAdminUrl => config
            .database
            .admin_url
            .clone()
            .unwrap_or_else(|| "(same as runtime)".to_string()),
        SettingsField::DatabasePoolMin => config.database.pool_min.to_string(),
        SettingsField::DatabasePoolMax => config.database.pool_max.to_string(),
        SettingsField::DatabaseConnectTimeout => {
            format!("{}s", config.database.connect_timeout_secs)
        }
        SettingsField::DefaultPosture => config.permissions.default_posture.clone(),
        SettingsField::AutoApproveCount => config.permissions.auto_approve.len().to_string(),
        SettingsField::AlwaysDenyCount => config.permissions.always_deny.len().to_string(),
        SettingsField::DaemonAutoConnect => bool_label(config.daemon.auto_connect),
    }
}

fn editable_value(config: &MoaConfig, field: SettingsField) -> Option<String> {
    match field {
        SettingsField::DefaultModel => Some(config.general.default_model.clone()),
        SettingsField::WorkspaceInstructions => Some(
            config
                .general
                .workspace_instructions
                .clone()
                .unwrap_or_default(),
        ),
        SettingsField::UserInstructions => {
            Some(config.general.user_instructions.clone().unwrap_or_default())
        }
        SettingsField::SandboxDir => Some(config.local.sandbox_dir.clone()),
        SettingsField::MemoryDir => Some(config.local.memory_dir.clone()),
        SettingsField::DatabaseUrl => Some(config.database.url.clone()),
        SettingsField::DatabaseAdminUrl => {
            Some(config.database.admin_url.clone().unwrap_or_default())
        }
        _ => None,
    }
}

fn field_footer(field: SettingsField) -> &'static str {
    if editable_field(field) {
        "Enter edit  ←/→ cycle enums  h/l category  Esc close"
    } else if readonly_field(field) {
        "h/l category  ↑/↓ field  Esc close"
    } else {
        "←/→ cycle  h/l category  ↑/↓ field  Esc close"
    }
}

fn field_detail(field: SettingsField) -> &'static str {
    match field {
        SettingsField::DefaultProvider | SettingsField::DefaultModel => {
            "Changing provider/model starts a fresh session on the new model."
        }
        SettingsField::ReasoningEffort
        | SettingsField::WorkspaceInstructions
        | SettingsField::UserInstructions
        | SettingsField::DockerEnabled
        | SettingsField::SandboxDir
        | SettingsField::MemoryDir
        | SettingsField::DatabaseBackend
        | SettingsField::DatabaseUrl
        | SettingsField::DatabaseAdminUrl
        | SettingsField::DatabasePoolMin
        | SettingsField::DatabasePoolMax
        | SettingsField::DatabaseConnectTimeout
        | SettingsField::DefaultPosture
        | SettingsField::DaemonAutoConnect => {
            "* Saved immediately. Existing sessions may keep their current runtime wiring."
        }
        SettingsField::AutoApproveCount | SettingsField::AlwaysDenyCount => {
            "Read-only summary. Edit the config file for detailed rule lists."
        }
        SettingsField::Theme
        | SettingsField::SidebarAuto
        | SettingsField::TabLimit
        | SettingsField::DiffStyle => "Applies immediately in the TUI.",
    }
}

fn editable_field(field: SettingsField) -> bool {
    editable_value(&MoaConfig::default(), field).is_some()
}

fn readonly_field(field: SettingsField) -> bool {
    matches!(
        field,
        SettingsField::AutoApproveCount | SettingsField::AlwaysDenyCount
    )
}

fn field_needs_follow_up(field: SettingsField) -> bool {
    !matches!(
        field,
        SettingsField::Theme
            | SettingsField::SidebarAuto
            | SettingsField::TabLimit
            | SettingsField::DiffStyle
            | SettingsField::AutoApproveCount
            | SettingsField::AlwaysDenyCount
    )
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
            SettingsMutation {
                reload_model_path: true,
                notice: None,
            }
        }
        SettingsField::ReasoningEffort => {
            cycle_string(
                &mut config.general.reasoning_effort,
                &["low", "medium", "high"],
                forward,
            );
            follow_up_notice()
        }
        SettingsField::Theme => {
            cycle_string(
                &mut config.tui.theme,
                &["default", "dark", "light"],
                forward,
            );
            SettingsMutation::default()
        }
        SettingsField::SidebarAuto => {
            config.tui.sidebar_auto = !config.tui.sidebar_auto;
            SettingsMutation::default()
        }
        SettingsField::TabLimit => {
            cycle_usize(&mut config.tui.tab_limit, &[4, 6, 8, 10, 12, 16], forward);
            SettingsMutation::default()
        }
        SettingsField::DiffStyle => {
            cycle_string(
                &mut config.tui.diff_style,
                &["auto", "side-by-side", "unified"],
                forward,
            );
            SettingsMutation::default()
        }
        SettingsField::DockerEnabled => {
            config.local.docker_enabled = !config.local.docker_enabled;
            follow_up_notice()
        }
        SettingsField::DatabaseBackend => {
            config.database.backend = match config.database.backend {
                DatabaseBackend::Turso => DatabaseBackend::Postgres,
                DatabaseBackend::Postgres => DatabaseBackend::Turso,
            };
            follow_up_notice()
        }
        SettingsField::DatabasePoolMin => {
            cycle_u32(&mut config.database.pool_min, &[1, 2, 5, 10], forward);
            if config.database.pool_max < config.database.pool_min {
                config.database.pool_max = config.database.pool_min;
            }
            follow_up_notice()
        }
        SettingsField::DatabasePoolMax => {
            cycle_u32(&mut config.database.pool_max, &[1, 5, 10, 20], forward);
            if config.database.pool_max < config.database.pool_min {
                config.database.pool_min = config.database.pool_max;
            }
            follow_up_notice()
        }
        SettingsField::DatabaseConnectTimeout => {
            cycle_u64(
                &mut config.database.connect_timeout_secs,
                &[5, 10, 20, 30],
                forward,
            );
            follow_up_notice()
        }
        SettingsField::DefaultPosture => {
            cycle_string(
                &mut config.permissions.default_posture,
                &["approve", "auto", "full", "deny"],
                forward,
            );
            follow_up_notice()
        }
        SettingsField::DaemonAutoConnect => {
            config.daemon.auto_connect = !config.daemon.auto_connect;
            follow_up_notice()
        }
        SettingsField::DefaultModel
        | SettingsField::WorkspaceInstructions
        | SettingsField::UserInstructions
        | SettingsField::SandboxDir
        | SettingsField::MemoryDir
        | SettingsField::DatabaseUrl
        | SettingsField::DatabaseAdminUrl
        | SettingsField::AutoApproveCount
        | SettingsField::AlwaysDenyCount => SettingsMutation::default(),
    }
}

fn apply_text_edit(
    config: &mut MoaConfig,
    field: SettingsField,
    draft: String,
) -> SettingsMutation {
    match field {
        SettingsField::DefaultModel => {
            let trimmed = draft.trim();
            if !trimmed.is_empty() {
                config.general.default_model = trimmed.to_string();
            }
            SettingsMutation {
                reload_model_path: true,
                notice: None,
            }
        }
        SettingsField::WorkspaceInstructions => {
            config.general.workspace_instructions = normalized_optional_text(draft);
            follow_up_notice()
        }
        SettingsField::UserInstructions => {
            config.general.user_instructions = normalized_optional_text(draft);
            follow_up_notice()
        }
        SettingsField::SandboxDir => {
            if !draft.trim().is_empty() {
                config.local.sandbox_dir = draft.trim().to_string();
            }
            follow_up_notice()
        }
        SettingsField::MemoryDir => {
            if !draft.trim().is_empty() {
                config.local.memory_dir = draft.trim().to_string();
            }
            follow_up_notice()
        }
        SettingsField::DatabaseUrl => {
            if !draft.trim().is_empty() {
                config.database.url = draft.trim().to_string();
            }
            follow_up_notice()
        }
        SettingsField::DatabaseAdminUrl => {
            config.database.admin_url = normalized_optional_text(draft);
            follow_up_notice()
        }
        _ => SettingsMutation::default(),
    }
}

fn provider_default_model(provider: &str) -> String {
    match provider {
        "anthropic" => "claude-sonnet-4-5".to_string(),
        "openrouter" => "openai/gpt-5.4".to_string(),
        _ => "gpt-5.4".to_string(),
    }
}

fn cycle_string(current: &mut String, values: &[&str], forward: bool) {
    let index = values
        .iter()
        .position(|candidate| current == candidate)
        .unwrap_or_default();
    let next = if forward {
        (index + 1) % values.len()
    } else if index == 0 {
        values.len() - 1
    } else {
        index - 1
    };
    *current = values[next].to_string();
}

fn cycle_usize(current: &mut usize, values: &[usize], forward: bool) {
    let index = values
        .iter()
        .position(|candidate| current == candidate)
        .unwrap_or_default();
    let next = if forward {
        (index + 1) % values.len()
    } else if index == 0 {
        values.len() - 1
    } else {
        index - 1
    };
    *current = values[next];
}

fn cycle_u32(current: &mut u32, values: &[u32], forward: bool) {
    let index = values
        .iter()
        .position(|candidate| current == candidate)
        .unwrap_or_default();
    let next = if forward {
        (index + 1) % values.len()
    } else if index == 0 {
        values.len() - 1
    } else {
        index - 1
    };
    *current = values[next];
}

fn cycle_u64(current: &mut u64, values: &[u64], forward: bool) {
    let index = values
        .iter()
        .position(|candidate| current == candidate)
        .unwrap_or_default();
    let next = if forward {
        (index + 1) % values.len()
    } else if index == 0 {
        values.len() - 1
    } else {
        index - 1
    };
    *current = values[next];
}

fn summarize_optional_text(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "(empty)".to_string();
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "(empty)".to_string();
    }
    let single_line = trimmed.replace('\n', " ");
    let summary = single_line.chars().take(48).collect::<String>();
    if single_line.chars().count() > 48 {
        format!("{summary}…")
    } else {
        summary
    }
}

fn normalized_optional_text(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn bool_label(value: bool) -> String {
    if value {
        "on".to_string()
    } else {
        "off".to_string()
    }
}

fn follow_up_notice() -> SettingsMutation {
    SettingsMutation {
        reload_model_path: false,
        notice: Some("Saved. Existing sessions keep their current runtime wiring.".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{SettingsField, SettingsViewState, apply_text_edit, field_value};
    use moa_core::MoaConfig;

    #[test]
    fn text_fields_enter_edit_mode_and_commit() {
        let mut state = SettingsViewState::new();
        let mut config = MoaConfig::default();
        state.move_down(&config);
        assert!(state.begin_edit(&config));
        state.push_edit_char('x');
        let mutation = state.commit_edit(&mut config);
        assert!(mutation.reload_model_path);
        assert_eq!(config.general.default_model, "gpt-5.4x");
    }

    #[test]
    fn optional_text_fields_round_trip_to_none_when_cleared() {
        let mut config = MoaConfig::default();
        config.general.workspace_instructions = Some("keep me".to_string());
        let mutation = apply_text_edit(
            &mut config,
            SettingsField::WorkspaceInstructions,
            "   ".to_string(),
        );
        assert!(!mutation.reload_model_path);
        assert_eq!(config.general.workspace_instructions, None);
    }

    #[test]
    fn rendered_value_summarizes_long_text_fields() {
        let mut config = MoaConfig::default();
        config.general.user_instructions = Some(
            "This is a long instruction that should be summarized in the settings list."
                .to_string(),
        );
        assert!(field_value(&config, SettingsField::UserInstructions).ends_with('…'));
    }
}
