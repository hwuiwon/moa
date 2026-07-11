//! Slack channel adapter built on top of `slack-morphism` Socket Mode.

mod adapter;
mod chunking;
mod error;
mod inbound;
mod refs;

pub use adapter::SlackAdapter;
pub use error::{SlackApiFailure, SlackApiFailureClass};
pub use inbound::{normalize_event_json, normalize_push_event};

#[cfg(test)]
mod tests;
