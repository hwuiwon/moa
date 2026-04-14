//! Root MOA application view composing titlebar + workspace + status bar.
//!
//! When Settings is open, the workspace is **replaced** (not layered)
//! with the full-page [`SettingsPage`]. The command palette still renders
//! as an overlay on top.

use gpui::{
    AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    Styled, Window, div, prelude::*,
};

use crate::{
    actions::{
        NewSession, OpenCommandPalette, OpenSettings, Quit, ToggleDetailPanel, ToggleSidebar,
    },
    layout::Workspace,
    panels::{
        command_palette::{CommandPalette, PaletteDismissed},
        settings::{SettingsDismissed, SettingsPage},
    },
    services::ServiceBridgeHandle,
    statusbar::MoaStatusBar,
    titlebar::MoaTitleBar,
    window_state::WindowState,
};

/// Top-level application view for the MOA desktop app.
pub struct MoaApp {
    titlebar: Entity<MoaTitleBar>,
    workspace: Entity<Workspace>,
    statusbar: Entity<MoaStatusBar>,
    palette: Option<Entity<CommandPalette>>,
    settings: Option<Entity<SettingsPage>>,
    focus: FocusHandle,
    window_state: WindowState,
}

impl Focusable for MoaApp {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl MoaApp {
    /// Creates the root application view.
    pub fn new(window_state: WindowState, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let bridge = ServiceBridgeHandle::global(cx);
        let workspace = cx.new(|cx| {
            let mut ws = Workspace::new(bridge.clone(), window, cx);
            ws.apply_state(&window_state);
            ws
        });

        // Observe workspace visibility changes and persist them.
        cx.observe(&workspace, |this, ws, cx| {
            let snapshot = ws.read(cx);
            this.window_state.sidebar_visible = snapshot.sidebar_visible();
            this.window_state.detail_visible = snapshot.detail_visible();
            this.window_state.save_to_default_path();
        })
        .detach();

        Self {
            titlebar: cx.new(|cx| MoaTitleBar::new(workspace.clone(), cx)),
            workspace,
            statusbar: cx.new(|cx| MoaStatusBar::new(bridge, cx)),
            palette: None,
            settings: None,
            focus: cx.focus_handle(),
            window_state,
        }
    }

    fn on_open_palette(
        &mut self,
        _: &OpenCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_palette(window, cx);
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Toggle: if already open, close it.
        if self.palette.is_some() {
            self.palette = None;
            cx.notify();
            return;
        }
        let palette = cx.new(|cx| CommandPalette::new(window, cx));
        cx.subscribe(&palette, |this, _palette, _event: &PaletteDismissed, cx| {
            this.palette = None;
            cx.notify();
        })
        .detach();
        // Give the palette keyboard focus so its scoped keybindings apply.
        palette.read(cx).focus_handle(cx).focus(window);
        self.palette = Some(palette);
        cx.notify();
    }

    fn on_open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(window, cx);
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            self.settings = None;
            cx.notify();
            return;
        }
        let bridge = ServiceBridgeHandle::global(cx);
        let page = cx.new(|cx| SettingsPage::new(bridge, window, cx));
        cx.subscribe(&page, |this, _page, _event: &SettingsDismissed, cx| {
            this.settings = None;
            cx.notify();
        })
        .detach();
        // Focus the settings page so its SettingsPage-scoped Esc binding
        // can fire.
        page.read(cx).focus_handle(cx).focus(window);
        self.settings = Some(page);
        cx.notify();
    }

    fn on_quit(&mut self, _: &Quit, _window: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    // Overlay-aware forwarding: actions dispatched from inside the command
    // palette (a sibling of the workspace, not an ancestor) would otherwise
    // bubble past MoaApp without reaching the workspace. Handling them here
    // and delegating ensures palette-confirmed commands work identically to
    // direct keypresses.
    fn on_toggle_sidebar(
        &mut self,
        _: &ToggleSidebar,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace.update(cx, |ws, cx| ws.toggle_sidebar(cx));
    }

    fn on_toggle_detail(
        &mut self,
        _: &ToggleDetailPanel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace.update(cx, |ws, cx| ws.toggle_detail(cx));
    }

    fn on_new_session(&mut self, _: &NewSession, _window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |ws, cx| ws.create_session(cx));
    }
}

impl Render for MoaApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Settings replaces the workspace; the titlebar and status bar
        // stay visible so the user always has context (close window, etc).
        let main: gpui::AnyElement = match self.settings.clone() {
            Some(settings) => settings.into_any_element(),
            None => self.workspace.clone().into_any_element(),
        };

        let mut root = div()
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .on_action(cx.listener(Self::on_open_palette))
            .on_action(cx.listener(Self::on_open_settings))
            .on_action(cx.listener(Self::on_quit))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_toggle_detail))
            .on_action(cx.listener(Self::on_new_session))
            .child(self.titlebar.clone())
            .child(div().flex().flex_col().flex_1().min_h_0().child(main))
            .child(self.statusbar.clone());

        if let Some(palette) = self.palette.clone() {
            root = root.child(palette);
        }
        root
    }
}
