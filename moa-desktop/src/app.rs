//! Root MOA application view composing titlebar + workspace + status bar.

use gpui::{AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div};

use crate::{layout::Workspace, statusbar::MoaStatusBar, titlebar::MoaTitleBar};

/// Top-level application view for the MOA desktop app.
pub struct MoaApp {
    titlebar: Entity<MoaTitleBar>,
    workspace: Entity<Workspace>,
    statusbar: Entity<MoaStatusBar>,
}

impl MoaApp {
    /// Creates the root application view.
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = cx.new(Workspace::new);
        Self {
            titlebar: cx.new(|cx| MoaTitleBar::new(workspace.clone(), cx)),
            workspace,
            statusbar: cx.new(MoaStatusBar::new),
        }
    }
}

impl Render for MoaApp {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(self.titlebar.clone())
            .child(self.workspace.clone())
            .child(self.statusbar.clone())
    }
}
