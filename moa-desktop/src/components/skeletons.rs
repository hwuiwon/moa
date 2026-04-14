//! Shared skeleton builders for async-loading panels.
//!
//! Panels that were previously showing a plain "Loading…" string use these
//! placeholders to signal activity with the same shape as the final
//! content. `gpui_component::Skeleton` handles the pulse animation; we
//! just size and compose them.

use gpui::{IntoElement, ParentElement, Styled, div, px};
use gpui_component::skeleton::Skeleton;

/// A vertical stack of session-row skeletons for the sidebar list.
pub fn session_rows(count: usize) -> impl IntoElement {
    let mut list = div().flex().flex_col().gap_2().p_3();
    for _ in 0..count {
        list = list.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(Skeleton::new().w(px(140.0)).h(px(12.0)))
                .child(Skeleton::new().secondary().w(px(200.0)).h(px(10.0))),
        );
    }
    list
}

/// A vertical stack of chat-message skeletons.
pub fn chat_messages(count: usize) -> impl IntoElement {
    let mut list = div().flex().flex_col().gap_3().p_4();
    for i in 0..count {
        let is_user = i % 2 == 0;
        let (primary, secondary) = if is_user {
            (px(220.0), px(180.0))
        } else {
            (px(420.0), px(380.0))
        };
        list = list.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(Skeleton::new().secondary().w(px(60.0)).h(px(10.0)))
                .child(Skeleton::new().w(primary).h(px(14.0)))
                .child(Skeleton::new().w(secondary).h(px(14.0))),
        );
    }
    list
}

/// A vertical stack of memory-tree skeletons.
pub fn memory_rows(count: usize) -> impl IntoElement {
    let mut list = div().flex().flex_col().gap_2().p_3();
    for i in 0..count {
        let indent = px((i % 3) as f32 * 12.0);
        list = list.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .pl(indent)
                .child(Skeleton::new().w(px(12.0)).h(px(12.0)).rounded_sm())
                .child(Skeleton::new().w(px(160.0)).h(px(10.0))),
        );
    }
    list
}

/// A paragraph-shaped skeleton for the memory-page viewer.
pub fn memory_page() -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .child(Skeleton::new().w(px(240.0)).h(px(20.0)))
        .child(Skeleton::new().secondary().w(px(120.0)).h(px(12.0)))
        .child(div().h(px(16.0)))
        .child(Skeleton::new().w_full().h(px(12.0)))
        .child(Skeleton::new().w_full().h(px(12.0)))
        .child(Skeleton::new().w(px(380.0)).h(px(12.0)))
        .child(div().h(px(12.0)))
        .child(Skeleton::new().w_full().h(px(12.0)))
        .child(Skeleton::new().w(px(300.0)).h(px(12.0)))
}

/// Timeline-node skeletons for the detail panel.
pub fn timeline_nodes(count: usize) -> impl IntoElement {
    let mut list = div().flex().flex_col().gap_2().p_2();
    for _ in 0..count {
        list = list.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(Skeleton::new().w(px(16.0)).h(px(16.0)).rounded_full())
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .flex_1()
                        .child(Skeleton::new().w(px(180.0)).h(px(10.0)))
                        .child(
                            Skeleton::new()
                                .secondary()
                                .w(px(140.0))
                                .h(px(8.0)),
                        ),
                ),
        );
    }
    list
}

