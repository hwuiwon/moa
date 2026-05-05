//! Centralized badge components for status, confidence, risk, and tool
//! outcomes. Replaces the duplicated hand-rolled badges previously living
//! in `session_row.rs`, `memory_list.rs`, `skill_list.rs`,
//! `memory_viewer.rs`, `message_bubble.rs`, and `detail_panel.rs`.
//!
//! Every color flows through `cx.theme().*` so dark/light parity and
//! WCAG audits work in one place. Adding a new status/confidence/risk
//! variant only requires updating the matcher here.

use gpui::{App, Hsla, IntoElement, ParentElement, SharedString, Styled, div, px};
use gpui_component::ActiveTheme;
use moa_core::{ConfidenceLevel, RiskLevel, SessionStatus};

// ---- session status -------------------------------------------------------

/// Color used to represent a [`SessionStatus`] in dots, badges, and
/// timeline markers. Reads from `cx.theme().*` so theme switching is free.
pub fn status_color(cx: &App, status: &SessionStatus) -> Hsla {
    let theme = cx.theme();
    match status {
        SessionStatus::Running => theme.info,
        SessionStatus::Completed => theme.success,
        SessionStatus::Failed => theme.danger,
        SessionStatus::WaitingApproval => theme.warning,
        SessionStatus::Created => theme.accent_foreground,
        SessionStatus::Paused | SessionStatus::Cancelled => theme.muted_foreground,
    }
}

pub fn status_label(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Created => "new",
        SessionStatus::Running => "running",
        SessionStatus::Paused => "paused",
        SessionStatus::WaitingApproval => "waiting approval",
        SessionStatus::Completed => "completed",
        SessionStatus::Failed => "failed",
        SessionStatus::Cancelled => "cancelled",
    }
}

/// Inline status badge: 6 px dot + muted text label.
#[allow(dead_code)]
pub fn status_badge(cx: &App, status: &SessionStatus) -> impl IntoElement + use<> {
    let theme = cx.theme().clone();
    let color = status_color(cx, status);
    let label = status_label(status);
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().size(px(6.0)).rounded_full().bg(color))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(SharedString::from(label.to_string())),
        )
}

// ---- confidence -----------------------------------------------------------

pub fn confidence_color(cx: &App, level: &ConfidenceLevel) -> Hsla {
    let theme = cx.theme();
    match level {
        ConfidenceLevel::High => theme.success,
        ConfidenceLevel::Medium => theme.warning,
        ConfidenceLevel::Low => theme.muted_foreground,
    }
}

pub fn confidence_label(level: &ConfidenceLevel) -> &'static str {
    match level {
        ConfidenceLevel::High => "high",
        ConfidenceLevel::Medium => "medium",
        ConfidenceLevel::Low => "low",
    }
}

/// Pill-style confidence badge for memory pages and skill cards.
pub fn confidence_badge(cx: &App, level: &ConfidenceLevel) -> impl IntoElement + use<> {
    let theme = cx.theme().clone();
    let dot = confidence_color(cx, level);
    let label = confidence_label(level);
    div()
        .flex()
        .items_center()
        .gap_1()
        .px_1p5()
        .py_0p5()
        .rounded_sm()
        .bg(theme.muted)
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(div().size(px(5.0)).rounded_full().bg(dot))
        .child(SharedString::from(label.to_string()))
}

// ---- risk -----------------------------------------------------------------

pub fn risk_color(cx: &App, level: &RiskLevel) -> Hsla {
    let theme = cx.theme();
    match level {
        RiskLevel::Low => theme.success,
        RiskLevel::Medium => theme.warning,
        RiskLevel::High => theme.danger,
    }
}

pub fn risk_label(level: &RiskLevel) -> &'static str {
    match level {
        RiskLevel::Low => "low risk",
        RiskLevel::Medium => "medium risk",
        RiskLevel::High => "high risk",
    }
}

/// Risk badge used inside approval cards and tool turn headers.
pub fn risk_badge(cx: &App, level: &RiskLevel) -> impl IntoElement + use<> {
    let theme = cx.theme().clone();
    let bg = risk_color(cx, level);
    div()
        .px_1p5()
        .py_0p5()
        .rounded_sm()
        .text_xs()
        .text_color(theme.background)
        .bg(bg)
        .child(SharedString::from(risk_label(level).to_string()))
}

// ---- tool outcome ---------------------------------------------------------

/// 6 px colored dot indicating tool success/failure. Used in tool cards.
#[allow(dead_code)]
pub fn tool_outcome_dot(cx: &App, success: bool) -> impl IntoElement + use<> {
    let theme = cx.theme();
    let color = if success { theme.success } else { theme.danger };
    div().size(px(6.0)).rounded_full().bg(color)
}
