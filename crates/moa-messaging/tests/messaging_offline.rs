//! Consolidated offline messaging integration tests.

#[path = "messaging_offline/char_limits.rs"]
mod char_limits;
#[path = "messaging_offline/control_signals.rs"]
mod control_signals;
#[cfg(any(feature = "postmark", feature = "twilio"))]
#[path = "messaging_offline/delivery_offline.rs"]
mod delivery_offline;
#[path = "messaging_offline/edit_window.rs"]
mod edit_window;
#[path = "messaging_offline/normalization.rs"]
mod normalization;
#[cfg(feature = "postmark")]
#[path = "messaging_offline/postmark_offline.rs"]
mod postmark_offline;
#[path = "messaging_offline/rate_limiting.rs"]
mod rate_limiting;
#[cfg(feature = "twilio")]
#[path = "messaging_offline/twilio_offline.rs"]
mod twilio_offline;
