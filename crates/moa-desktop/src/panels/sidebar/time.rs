//! Relative timestamp formatting helpers.

use chrono::{DateTime, Utc};

/// Renders a `DateTime<Utc>` as a short human-readable relative label.
pub fn relative(timestamp: DateTime<Utc>) -> String {
    let now = Utc::now();
    let delta = now.signed_duration_since(timestamp);
    let secs = delta.num_seconds();
    if secs < 45 {
        return "just now".to_string();
    }
    let minutes = delta.num_minutes();
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = delta.num_hours();
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = delta.num_days();
    if days < 2 {
        return "yesterday".to_string();
    }
    if days < 7 {
        return format!("{days}d ago");
    }
    timestamp.format("%Y-%m-%d").to_string()
}
