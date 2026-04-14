//! Full-page Settings view. Replaces the workspace in `MoaApp::render`
//! when `settings: Some(...)` — it is no longer a modal overlay. See
//! `design.md` → "Settings chrome" for layout spec.
//!
//! Layout:
//! ```
//! ┌── BackBar "← Back to app" ─────────────────────────────┐
//! │                                                         │
//! │ ┌──────────────┬────────────────────────────────────┐  │
//! │ │  Left nav    │  Section title + description       │  │
//! │ │  General     │                                    │  │
//! │ │  Appearance  │  [ section_card ]                  │  │
//! │ │  Providers   │  [ section_card ]                  │  │
//! │ │  Permissions │                                    │  │
//! │ │  Shortcuts   │                                    │  │
//! │ └──────────────┴────────────────────────────────────┘  │
//! └────────────────────────────────────────────────────────┘
//! ```

use std::path::PathBuf;

use gpui::{
    AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, KeyContext,
    MouseButton, ParentElement, Render, SharedString, Styled, Window, div, prelude::*, px, rems,
};
use gpui_component::{
    ActiveTheme,
    input::{InputEvent, InputState},
};
use moa_core::MoaConfig;

use crate::{actions::BackToApp, components::nav::nav_item, services::ServiceBridgeHandle};

use super::{
    appearance_tab::render_appearance_tab, general_tab::render_general_tab,
    keyboard_shortcuts_tab::render_keyboard_shortcuts_tab, permissions_tab::render_permissions_tab,
    providers_tab::render_providers_tab,
};

/// Emitted when the user leaves the Settings page.
#[derive(Clone, Debug)]
pub struct SettingsDismissed;

/// Left-nav sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsSection {
    General,
    Appearance,
    Providers,
    Permissions,
    KeyboardShortcuts,
}

impl SettingsSection {
    const ALL: [SettingsSection; 5] = [
        Self::General,
        Self::Appearance,
        Self::Providers,
        Self::Permissions,
        Self::KeyboardShortcuts,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Providers => "Providers",
            Self::Permissions => "Permissions",
            Self::KeyboardShortcuts => "Keyboard Shortcuts",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::General => "Core app behavior and defaults.",
            Self::Appearance => "Theme and visual density.",
            Self::Providers => "API credentials for each LLM provider.",
            Self::Permissions => "Tool approval posture and auto-allow lists.",
            Self::KeyboardShortcuts => "Reference of every registered shortcut.",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::General => "nav-general",
            Self::Appearance => "nav-appearance",
            Self::Providers => "nav-providers",
            Self::Permissions => "nav-permissions",
            Self::KeyboardShortcuts => "nav-keyboard",
        }
    }
}

/// Settings-page state.
pub struct SettingsPage {
    bridge: ServiceBridgeHandle,
    config: MoaConfig,
    config_path: PathBuf,
    active_section: SettingsSection,
    status: Option<SharedString>,
    error: Option<SharedString>,
    focus: FocusHandle,
    pub(super) auto_approve_input: Entity<InputState>,
    pub(super) always_deny_input: Entity<InputState>,
}

impl EventEmitter<SettingsDismissed> for SettingsPage {}

impl Focusable for SettingsPage {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SettingsPage {
    pub fn new(bridge: ServiceBridgeHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let config = bridge
            .entity()
            .read(cx)
            .config()
            .cloned()
            .unwrap_or_default();
        let config_path = MoaConfig::default_path().unwrap_or_else(|_| {
            // The literal "~/..." string would create a path with `~`
            // as a real directory name; expand it via $HOME.
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".moa/config.toml"))
                .unwrap_or_else(|| PathBuf::from(".moa/config.toml"))
        });

        let auto_approve_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("tool name (e.g. file_read) — Enter to add")
        });
        cx.subscribe(
            &auto_approve_input,
            |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let raw = input.read(cx).text().to_string();
                    let value = raw.trim().to_string();
                    if !value.is_empty() {
                        this.mutate(cx, |cfg| {
                            if !cfg.permissions.auto_approve.contains(&value) {
                                cfg.permissions.auto_approve.push(value.clone());
                            }
                        });
                    }
                }
            },
        )
        .detach();

        let always_deny_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("tool name — Enter to add to deny list")
        });
        cx.subscribe(&always_deny_input, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                let raw = input.read(cx).text().to_string();
                let value = raw.trim().to_string();
                if !value.is_empty() {
                    this.mutate(cx, |cfg| {
                        if !cfg.permissions.always_deny.contains(&value) {
                            cfg.permissions.always_deny.push(value.clone());
                        }
                    });
                }
            }
        })
        .detach();

        Self {
            bridge,
            config,
            config_path,
            active_section: SettingsSection::General,
            status: None,
            error: None,
            focus: cx.focus_handle(),
            auto_approve_input,
            always_deny_input,
        }
    }

    pub fn config(&self) -> &MoaConfig {
        &self.config
    }

    pub fn mutate(&mut self, cx: &mut Context<Self>, mutator: impl FnOnce(&mut MoaConfig)) {
        mutator(&mut self.config);
        self.persist(cx);
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        match self.config.save_to_path(&self.config_path) {
            Ok(()) => {
                self.status = Some("Saved".into());
                self.error = None;
                tracing::debug!(path = %self.config_path.display(), "saved moa config");
            }
            Err(err) => {
                self.status = None;
                self.error = Some(format!("save failed: {err:#}").into());
                tracing::warn!(%err, "failed to save moa config");
            }
        }
        cx.notify();
    }

    #[allow(dead_code)]
    pub(super) fn bridge(&self) -> &ServiceBridgeHandle {
        &self.bridge
    }

    fn set_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        if self.active_section != section {
            self.active_section = section;
            cx.notify();
        }
    }

    fn on_back(&mut self, _: &BackToApp, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SettingsDismissed);
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let active = self.active_section;

        // Back bar along the top.
        let back_bar = div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(40.0))
            .px_3()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(
                div()
                    .id("settings-back")
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .hover(|s| s.bg(theme.muted).text_color(theme.foreground))
                    .child("← Back to app")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.emit(SettingsDismissed)),
                    ),
            );

        // Left nav.
        let mut nav = div()
            .flex()
            .flex_col()
            .gap_0p5()
            .w(px(240.0))
            .p_3()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.sidebar);
        for section in SettingsSection::ALL {
            let is_active = active == section;
            let s = section;
            nav = nav.child(nav_item(
                cx,
                section.id(),
                section.label(),
                is_active,
                cx.listener(move |this, _, _, cx| this.set_section(s, cx)),
            ));
        }

        // Right content: section heading + body.
        let body = match active {
            SettingsSection::General => render_general_tab(self, window, cx),
            SettingsSection::Appearance => render_appearance_tab(self, cx),
            SettingsSection::Providers => render_providers_tab(self, cx),
            SettingsSection::Permissions => render_permissions_tab(self, window, cx),
            SettingsSection::KeyboardShortcuts => render_keyboard_shortcuts_tab(cx),
        };

        let heading = div()
            .flex()
            .flex_col()
            .gap_0p5()
            .pb_3()
            .child(
                div()
                    .text_size(rems(1.25))
                    .text_color(theme.foreground)
                    .child(active.label()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(active.description()),
            );

        let content = div()
            .id("settings-content")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .p_6()
            .gap_4()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .w_full()
                    .max_w(px(720.0))
                    .child(heading)
                    .child(body),
            );

        // Keyboard context: `SettingsPage` so Esc binds to BackToApp here
        // instead of DismissModal.
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("SettingsPage");

        let status_bar = div()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .py_1p5()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .text_xs()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when_some(self.status.clone(), |d, status| {
                        d.child(div().text_color(theme.muted_foreground).child(status))
                    })
                    .when_some(self.error.clone(), |d, err| {
                        d.child(div().text_color(theme.danger).child(err))
                    }),
            )
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child("Esc to go back"),
            );

        div()
            .key_context(key_context)
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(theme.background)
            .on_action(cx.listener(Self::on_back))
            .child(back_bar)
            .child(div().flex().flex_1().min_h_0().child(nav).child(content))
            .child(status_bar)
    }
}
