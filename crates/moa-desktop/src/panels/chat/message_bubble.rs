//! Individual message bubble renderer. Agent messages use gpui-component's
//! [`TextView::markdown`] for Markdown + code-block rendering; other variants
//! render as styled divs.

use std::collections::HashSet;

use gpui::{
    App, ClickEvent, Context, ElementId, ParentElement, SharedString, Styled, Window, div,
    prelude::*, px,
};
use gpui_component::{ActiveTheme, text::TextView};
use moa_core::{ApprovalDecision, ApprovalPrompt};
use uuid::Uuid;

use super::chat_panel::ChatPanel;
use super::messages::{ChatMessage, ToolInvocation};

/// Renders a single [`ChatMessage`] as an element.
pub fn render_message(
    index: usize,
    message: &ChatMessage,
    expanded_tools: &HashSet<Uuid>,
    window: &mut Window,
    cx: &mut Context<ChatPanel>,
) -> gpui::AnyElement {
    match message {
        ChatMessage::User { text, .. } => user_bubble(text, cx).into_any_element(),
        ChatMessage::Agent {
            text,
            model,
            input_tokens,
            output_tokens,
            cost_cents,
            ..
        } => agent_bubble(
            index,
            text,
            model,
            *input_tokens,
            *output_tokens,
            *cost_cents,
            window,
            cx,
        )
        .into_any_element(),
        ChatMessage::Thinking { summary, .. } => thinking_bubble(summary, cx).into_any_element(),
        ChatMessage::System { text, .. } => system_bubble(text, cx).into_any_element(),
        ChatMessage::Error {
            text, recoverable, ..
        } => error_bubble(text, *recoverable, cx).into_any_element(),
        ChatMessage::ToolTurn { calls, .. } => {
            tool_turn(calls, expanded_tools, cx).into_any_element()
        }
        ChatMessage::Approval {
            prompt,
            decision,
            decided_by,
            ..
        } => approval_card(prompt, decision.as_ref(), decided_by.as_deref(), cx).into_any_element(),
    }
}

fn user_bubble(text: &str, cx: &App) -> impl IntoElement + use<> {
    let theme = cx.theme();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_md()
        .bg(theme.muted)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("You"),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.foreground)
                .child(SharedString::from(text.to_string())),
        )
}

#[allow(clippy::too_many_arguments)]
fn agent_bubble(
    index: usize,
    text: &str,
    model: &str,
    input_tokens: usize,
    output_tokens: usize,
    cost_cents: u32,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement + use<> {
    let theme = cx.theme().clone();
    let id = ("chat-md", index as u64);
    let md = TextView::markdown(id, SharedString::from(text.to_string()), window, cx)
        .style(crate::components::markdown::markdown_style(cx))
        .selectable(true);

    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_md()
        .bg(theme.background)
        .child(
            // Header: just the model name as a small subdued pill at top-
            // left of the bubble.
            div().flex().items_center().gap_2().child(
                div()
                    .text_xs()
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .bg(theme.muted)
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(model.to_string())),
            ),
        )
        .child(
            // Line-height tracks the active density (1.55 comfortable, 1.4
            // compact). Set on the wrapper because `TextViewStyle` doesn't
            // expose line-height directly.
            div()
                .text_sm()
                .text_color(theme.foreground)
                .line_height(crate::density::current(cx).spacing().markdown_line_height)
                .child(md),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!(
                    "in {input_tokens} · out {output_tokens} · ${:.4}",
                    cost_cents as f64 / 10000.0
                )),
        )
}

fn thinking_bubble(summary: &str, cx: &App) -> impl IntoElement + use<> {
    let theme = cx.theme();
    div()
        .flex()
        .items_center()
        .gap_2()
        .p_2()
        .rounded_md()
        .bg(theme.muted)
        .child(div().size(px(6.)).rounded_full().bg(theme.warning))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(SharedString::from(summary.to_string())),
        )
}

fn system_bubble(text: &str, cx: &App) -> impl IntoElement + use<> {
    let theme = cx.theme();
    div()
        .flex()
        .items_center()
        .justify_center()
        .py_1()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(SharedString::from(text.to_string()))
}

fn error_bubble(text: &str, recoverable: bool, cx: &App) -> impl IntoElement + use<> {
    let theme = cx.theme();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_md()
        .border_l_2()
        .border_color(theme.danger)
        .bg(theme.muted)
        .child(
            div()
                .text_xs()
                .text_color(theme.danger)
                .child(if recoverable {
                    "Error (recoverable)"
                } else {
                    "Error"
                }),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.foreground)
                .child(SharedString::from(text.to_string())),
        )
}

fn tool_turn(
    calls: &[ToolInvocation],
    expanded_tools: &HashSet<Uuid>,
    cx: &mut Context<ChatPanel>,
) -> impl IntoElement + use<> {
    let theme = cx.theme().clone();

    // Vertical thread line on the left, spanning the whole tool group.
    // No background or border around the outer container — the chat
    // panel's own spacing does the job of delimiting.
    let mut column = div()
        .flex()
        .flex_col()
        .gap_1()
        .pl_4()
        .border_l_1()
        .border_color(theme.border);
    for call in calls {
        column = column.child(tool_card(call, expanded_tools.contains(&call.tool_id), cx));
    }

    // "Done" footer appears once every call in the turn has resolved
    // (success=Some(_)). Mirrors the reference screenshot: a small check
    // icon + muted label attached to the same thread line.
    let all_resolved = !calls.is_empty() && calls.iter().all(|c| c.success.is_some());
    if all_resolved {
        column = column.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .pt_1()
                .child(
                    // Check-style indicator built from a tiny circle
                    // outline — no icon asset required.
                    div()
                        .size(px(12.0))
                        .rounded_full()
                        .border_1()
                        .border_color(theme.success)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(div().size(px(4.0)).rounded_full().bg(theme.success)),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Done"),
                ),
        );
    }
    column
}

fn tool_card(
    call: &ToolInvocation,
    expanded: bool,
    cx: &mut Context<ChatPanel>,
) -> impl IntoElement + use<> {
    let theme = cx.theme().clone();
    let tool_id = call.tool_id;

    // Leading indicator: hollow circle while pending, filled when done.
    // Matches the reference's left-rail glyph without a full icon set.
    let indicator = {
        let mut d = div()
            .size(px(12.0))
            .rounded_full()
            .border_1()
            .flex()
            .items_center()
            .justify_center();
        match call.success {
            Some(true) => {
                d = d
                    .border_color(theme.success)
                    .child(div().size(px(4.0)).rounded_full().bg(theme.success));
            }
            Some(false) => {
                d = d
                    .border_color(theme.danger)
                    .child(div().size(px(4.0)).rounded_full().bg(theme.danger));
            }
            None => {
                d = d.border_color(theme.muted_foreground);
            }
        }
        d
    };

    let risk_badge = call
        .risk_level
        .as_ref()
        .map(|r| crate::components::badges::risk_badge(cx, r));

    // Single-line header row. No chevron — the whole row is clickable.
    let header = div()
        .id(ElementId::NamedInteger(
            "tool-row".into(),
            tool_id.as_u128() as u64,
        ))
        .flex()
        .items_center()
        .gap_2()
        .py_0p5()
        .rounded_md()
        .hover(|s| s.bg(theme.muted))
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_tool(tool_id, cx)))
        .child(indicator)
        .child(
            div()
                .text_sm()
                .text_color(theme.foreground)
                .child(SharedString::from(call.tool_name.clone())),
        )
        .when_some(risk_badge, |row, badge| row.child(badge))
        .when_some(call.duration_ms, |row, ms| {
            row.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{ms}ms")),
            )
        });

    let mut column = div().flex().flex_col().gap_2().child(header);

    if expanded {
        // Indented detail panel: a rounded card with the request payload
        // (and optionally the response) rendered as fenced JSON so the
        // markdown renderer picks it up and applies syntax highlighting.
        if !call.input_preview.is_empty() {
            column = column.child(detail_card(
                cx,
                "Request",
                &call.input_preview,
                ("tool-req", tool_id.as_u128() as u64),
            ));
        }
        if let Some(output) = &call.output_preview {
            column = column.child(detail_card(
                cx,
                "Response",
                output,
                ("tool-res", tool_id.as_u128() as u64),
            ));
        }
    }

    column
}

/// Builds an indented preformatted card used for expanded tool I/O.
/// Renders the body as plain text — markdown rendering would require
/// threading a `Window` into this helper, which the call sites don't
/// currently provide.
fn detail_card(
    cx: &mut gpui::App,
    heading: &'static str,
    body: &str,
    id: (&'static str, u64),
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let _ = id;
    div()
        .ml(px(16.0))
        .p_3()
        .rounded_md()
        .bg(theme.muted)
        .border_1()
        .border_color(theme.border)
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(heading),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.foreground)
                .font_family("monospace")
                .whitespace_normal()
                .child(SharedString::from(body.to_string())),
        )
        .into_any_element()
}

fn approval_card(
    prompt: &ApprovalPrompt,
    decision: Option<&ApprovalDecision>,
    decided_by: Option<&str>,
    cx: &mut Context<ChatPanel>,
) -> impl IntoElement + use<> {
    let theme = cx.theme().clone();
    let request_id = prompt.request.request_id;
    let pattern = prompt.pattern.clone();
    let (border_color, status_label) = if decision.is_some() {
        (theme.border, "Decided")
    } else {
        (
            crate::components::badges::risk_color(cx, &prompt.request.risk_level),
            "Approval required",
        )
    };
    let header = div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(status_label.to_string()),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.foreground)
                .child(SharedString::from(prompt.request.tool_name.clone())),
        )
        .child(crate::components::badges::risk_badge(
            cx,
            &prompt.request.risk_level,
        ));

    let summary = div()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(SharedString::from(prompt.request.input_summary.clone()));

    let mut card = div()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_md()
        .bg(theme.background)
        .border_2()
        .border_color(border_color)
        .child(header)
        .child(summary);

    for field in &prompt.parameters {
        card = card.child(
            div()
                .flex()
                .gap_2()
                .text_xs()
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(SharedString::from(field.label.clone())),
                )
                .child(
                    div()
                        .text_color(theme.foreground)
                        .child(SharedString::from(field.value.clone())),
                ),
        );
    }

    if let Some(decision) = decision {
        let outcome = match decision {
            ApprovalDecision::AllowOnce => "Allowed once".to_string(),
            ApprovalDecision::AlwaysAllow { pattern } => format!("Always allow: {pattern}"),
            ApprovalDecision::Deny { reason } => match reason {
                Some(r) => format!("Denied: {r}"),
                None => "Denied".to_string(),
            },
        };
        card = card.child(div().text_xs().text_color(theme.muted_foreground).child(
            SharedString::from(match decided_by {
                Some(by) => format!("{outcome} · by {by}"),
                None => outcome,
            }),
        ));
    } else {
        let always_allow_pattern = pattern.clone();
        let req_suffix = request_id.as_u128() as u64;
        let buttons = div()
            .flex()
            .gap_2()
            .child(decision_button(
                ("approve-allow", req_suffix),
                "Allow",
                theme.success,
                theme.success_foreground,
                theme.success_hover,
                cx.listener(move |this, _, _, cx| {
                    this.decide_approval(request_id, ApprovalDecision::AllowOnce, "", cx);
                }),
            ))
            .child(decision_button(
                ("approve-always", req_suffix),
                "Always allow",
                theme.primary,
                theme.primary_foreground,
                theme.primary_hover,
                cx.listener(move |this, _, _, cx| {
                    this.decide_approval(
                        request_id,
                        ApprovalDecision::AlwaysAllow {
                            pattern: always_allow_pattern.clone(),
                        },
                        &always_allow_pattern,
                        cx,
                    );
                }),
            ))
            .child(decision_button(
                ("approve-deny", req_suffix),
                "Deny",
                theme.danger,
                theme.danger_foreground,
                theme.danger_hover,
                cx.listener(move |this, _, _, cx| {
                    this.decide_approval(
                        request_id,
                        ApprovalDecision::Deny { reason: None },
                        "",
                        cx,
                    );
                }),
            ));
        card = card.child(buttons);
    }
    card
}

fn decision_button(
    id: (&'static str, u64),
    label: &'static str,
    bg: gpui::Hsla,
    fg: gpui::Hsla,
    hover_bg: gpui::Hsla,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(ElementId::NamedInteger(id.0.into(), id.1))
        .px_3()
        .py_1()
        .rounded_md()
        .text_xs()
        .bg(bg)
        .text_color(fg)
        .hover(move |s| s.bg(hover_bg))
        .child(label.to_string())
        .on_click(on_click)
}
