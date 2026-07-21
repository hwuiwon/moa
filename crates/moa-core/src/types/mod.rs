//! Shared cross-crate DTOs, identifiers, and supporting enums.

#[macro_use]
mod macros;

pub mod action_policy;
pub mod agent;
pub mod channel;
pub mod completion;
pub mod contact;
pub mod context;
pub mod events_stream;
pub mod execution_planning;
pub mod experience;
pub mod guardrails;
pub mod hands;
pub mod identifiers;
pub mod learning;
pub mod memory;
pub mod model;
pub mod observability;
pub mod provider;
pub mod query_rewrite;
pub mod runtime_events;
pub mod security;
pub mod segment_assessment;
pub mod segments;
pub mod session;
pub mod skill_use;
pub mod snapshot;
pub mod tools;
pub mod worker;

#[cfg(test)]
mod tests {
    use crate::error::MoaError;

    #[test]
    fn cancelled_error_is_distinct() {
        assert_eq!(
            MoaError::Cancelled.to_string(),
            "operation cancelled by user"
        );
        assert!(!matches!(
            MoaError::Cancelled,
            MoaError::ProviderError(_) | MoaError::ToolError(_)
        ));
    }
}
