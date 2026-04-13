//! Shared cross-crate DTOs, identifiers, and supporting enums.

#[macro_use]
mod macros;

mod approval;
mod completion;
mod context;
mod events_stream;
mod hands;
mod identifiers;
mod memory;
mod model;
mod observability;
mod platform;
mod runtime_events;
mod scheduling;
mod session;
mod tools;

pub use approval::*;
pub use completion::*;
pub use context::*;
pub use events_stream::*;
pub use hands::*;
pub use identifiers::*;
pub use memory::*;
pub use model::*;
pub use observability::*;
pub use platform::*;
pub use runtime_events::*;
pub use scheduling::*;
pub use session::*;
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
