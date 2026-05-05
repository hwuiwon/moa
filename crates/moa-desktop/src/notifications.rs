//! Toast notification helpers built on gpui-component's `Notification`.
//!
//! Rather than build a bespoke toast manager, this module thin-wraps
//! gpui-component's built-in `push_notification` (which auto-hides,
//! caps visible count, and handles animation) so call sites get a small,
//! MOA-flavoured API while remaining compatible with the wider theme.

use gpui::{App, SharedString, Window};
use gpui_component::{WindowExt, notification::Notification};

/// Convenience: push an info-level toast.
#[allow(dead_code)]
pub fn info(window: &mut Window, cx: &mut App, message: impl Into<SharedString>) {
    window.push_notification(Notification::info(message), cx);
}

/// Convenience: push a success-level toast.
pub fn success(window: &mut Window, cx: &mut App, message: impl Into<SharedString>) {
    window.push_notification(Notification::success(message), cx);
}

/// Convenience: push a warning-level toast.
pub fn warning(window: &mut Window, cx: &mut App, message: impl Into<SharedString>) {
    window.push_notification(Notification::warning(message), cx);
}

/// Convenience: push an error-level toast.
#[allow(dead_code)]
pub fn error(window: &mut Window, cx: &mut App, message: impl Into<SharedString>) {
    window.push_notification(Notification::error(message), cx);
}

/// Push a one-shot toast that's sticky until the user dismisses it. Used
/// for rare, important events where auto-hide would lose information.
#[allow(dead_code)]
pub fn sticky_error(window: &mut Window, cx: &mut App, message: impl Into<SharedString>) {
    window.push_notification(Notification::error(message).autohide(false), cx);
}
