//! Owned Restate child-call cancellation and join support.

use restate_sdk::{
    context::{CallFuture, DurableFuture},
    prelude::{HandlerError, TerminalError},
};

/// Result of racing one attached child call against cooperative MOA cancellation.
pub(crate) enum ChildInvocationOutcome<T> {
    /// The child completed before cancellation took effect.
    Completed(T),
    /// Cancellation won and the exact child invocation reached a terminal state.
    Cancelled(String),
}

/// Races an owned child call against a durable cancellation future.
///
/// Unlike `restate_sdk::select!`, this retains the [`CallFuture`] after selection.
/// That ownership is required to cancel and then join the exact invocation before
/// the parent reports its normal product-level cancelled outcome. A successful join
/// remains completed when the child crossed its terminal boundary concurrently.
pub(crate) async fn cancel_and_join_child_call<C, F>(
    cancellation: C,
    call: F,
) -> Result<ChildInvocationOutcome<F::Response>, HandlerError>
where
    C: DurableFuture<Output = Result<String, TerminalError>> + Send,
    F: CallFuture + Send,
{
    let inner_context = cancellation.inner_context();
    let selector = inner_context.select(vec![cancellation.handle(), call.handle()]);
    match selector.await? {
        0 => {
            let reason = cancellation.await?;
            call.cancel().await?;
            resolve_cancelled_child_join(reason, call.await).map_err(HandlerError::from)
        }
        1 => Ok(ChildInvocationOutcome::Completed(call.await?)),
        index => Err(TerminalError::new(format!(
            "child invocation selector returned invalid branch {index}"
        ))
        .into()),
    }
}

fn resolve_cancelled_child_join<T>(
    reason: String,
    joined: Result<T, TerminalError>,
) -> Result<ChildInvocationOutcome<T>, TerminalError> {
    match joined {
        // A response that crossed the child's terminal boundary before Restate
        // applied cancellation is authoritative. Preserve it so callers retain
        // the provider's actual output and billed usage.
        Ok(response) => Ok(ChildInvocationOutcome::Completed(response)),
        // Restate reports an invocation stopped by explicit cancellation as 409.
        Err(error) if error.code() == 409 => Ok(ChildInvocationOutcome::Cancelled(reason)),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use moa_core::types::{
        completion::{CompletionResponse, StopReason, TokenUsage},
        identifiers::ModelId,
    };
    use restate_sdk::prelude::TerminalError;

    use super::{ChildInvocationOutcome, resolve_cancelled_child_join};

    #[test]
    fn completed_child_join_preserves_provider_response_at_cancellation_boundary_offline() {
        // Pins: when cancellation and a provider completion are simultaneously ready,
        // the completed provider response and its billed usage remain authoritative.
        let response = CompletionResponse {
            text: "completed response".to_string(),
            content: Vec::new(),
            stop_reason: StopReason::EndTurn,
            model: ModelId::new("provider-model"),
            usage: TokenUsage {
                input_tokens_uncached: 17,
                input_tokens_cache_write: 3,
                input_tokens_cache_read: 5,
                output_tokens: 11,
            },
            duration_ms: 23,
            thought_signature: None,
        };

        let outcome = resolve_cancelled_child_join(
            "cancel raced with completion".to_string(),
            Ok(response.clone()),
        )
        .expect("a completed child join should remain successful");

        match outcome {
            ChildInvocationOutcome::Completed(actual) => assert_eq!(actual, response),
            ChildInvocationOutcome::Cancelled(reason) => {
                panic!("completed provider response was discarded as cancellation: {reason}")
            }
        }
    }

    #[test]
    fn cancelled_child_join_returns_product_cancellation_offline() {
        // Pins: Restate's explicit invocation-cancel terminal response remains the
        // product-level cancellation outcome after the exact child is joined.
        let outcome = resolve_cancelled_child_join::<CompletionResponse>(
            "user requested cancellation".to_string(),
            Err(TerminalError::new_with_code(409, "invocation cancelled")),
        )
        .expect("an explicitly cancelled child should be a normal outcome");

        match outcome {
            ChildInvocationOutcome::Cancelled(reason) => {
                assert_eq!(reason, "user requested cancellation")
            }
            ChildInvocationOutcome::Completed(_) => {
                panic!("an explicitly cancelled child was reported as completed")
            }
        }
    }

    #[test]
    fn failed_child_join_propagates_non_cancellation_error_offline() {
        // Pins: a non-cancellation terminal failure cannot be rewritten as a
        // successful product cancellation merely because cancellation raced it.
        let error = match resolve_cancelled_child_join::<CompletionResponse>(
            "user requested cancellation".to_string(),
            Err(TerminalError::new_with_code(503, "provider unavailable")),
        ) {
            Ok(_) => panic!("a non-cancellation child failure was swallowed"),
            Err(error) => error,
        };

        assert_eq!(error.code(), 503);
        assert_eq!(error.message(), "provider unavailable");
    }
}
