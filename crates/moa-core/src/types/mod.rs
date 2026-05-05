//! Shared cross-crate DTOs, identifiers, and supporting enums.

#[macro_use]
mod macros;

mod approval;
mod completion;
mod context;
mod events_stream;
mod hands;
mod identifiers;
mod intents;
mod memory;
mod model;
mod observability;
mod platform;
mod provider;
mod query_rewrite;
mod resolution;
mod runtime_events;
mod scheduling;
mod segments;
mod session;
mod snapshot;
mod sub_agent;
mod tools;

pub use approval::*;
pub use completion::*;
pub use context::*;
pub use events_stream::*;
pub use hands::*;
pub use identifiers::*;
pub use intents::*;
pub use memory::*;
pub use model::*;
pub use observability::*;
pub use platform::*;
pub use provider::*;
pub use query_rewrite::*;
pub use resolution::*;
pub use runtime_events::*;
pub use scheduling::*;
pub use segments::*;
pub use session::*;
pub use snapshot::*;
pub use sub_agent::*;
pub use tools::*;

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
