//! Deadline and cancellation propagation for LLM provider calls.
//!
//! A provider call is the most expensive thing MOA dispatches, and it is the
//! one place where "cancelled" and "abandoned" are genuinely different. Wrapping
//! `provider.complete(request)` in a `tokio::time::timeout` drops the outer
//! future, but the inner completion task keeps streaming, the socket stays open,
//! and the tokens keep being billed until the provider decides the turn is over.
//! Worse, a caller that already knows the scope is dead still *sends* the
//! request: the cancellation only takes effect after the money is spent.
//!
//! [`CancellableLLMProvider`] closes both gaps around any [`LLMProvider`]:
//!
//! 1. **Before dispatch** it asks [`DeadlineGuard::admit`]. A cancelled or
//!    expired scope returns without touching the inner provider, so a cancelled
//!    turn issues exactly zero provider calls. This is a synchronous question
//!    rather than a `select!` race, because a `select!` still polls the request
//!    branch and a fast provider can win it.
//! 2. **During streaming** it interposes on the block stream. Cancellation or
//!    deadline expiry terminates the stream with a typed error *and* drops the
//!    inner [`CompletionStream`], whose `Drop` aborts the provider task that
//!    owns the HTTP response. Expiry also cancels the shared token, so sibling
//!    work under the same scope unwinds instead of being orphaned.
//!
//! The guard is per-scope, not per-process: callers construct one of these
//! around a resolved provider for the turn or trial they are about to run, which
//! keeps the whole thing additive — no existing `LLMProvider` signature moves.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    error::MoaError, error::Result, traits::LLMProvider, types::completion::CompletionRequest,
    types::completion::CompletionStream, types::completion::SharedCompletionRequest,
    types::model::ModelCapabilities, types::resource::DeadlineGuard,
    types::resource::ResourceBudget,
};

/// Buffered blocks held between the inner provider stream and the caller.
///
/// One is enough for correctness — the forwarding task is woken per block — but
/// a small buffer keeps a fast provider from stalling on every single block
/// while a slow consumer catches up.
const FORWARD_BUFFER_BLOCKS: usize = 8;

/// An [`LLMProvider`] whose calls are bounded by a [`DeadlineGuard`].
///
/// See the module documentation for what this enforces and why a dropped future
/// is not enough on its own.
pub struct CancellableLLMProvider {
    inner: Arc<dyn LLMProvider>,
    guard: DeadlineGuard,
}

impl CancellableLLMProvider {
    /// Binds a provider to one cancellation scope.
    #[must_use]
    pub fn new(inner: Arc<dyn LLMProvider>, guard: DeadlineGuard) -> Self {
        Self { inner, guard }
    }

    /// Returns the scope bounding every call through this provider.
    #[must_use]
    pub const fn guard(&self) -> &DeadlineGuard {
        &self.guard
    }

    /// Returns what this scope may still spend.
    #[must_use]
    pub const fn budget(&self) -> ResourceBudget {
        self.guard.budget()
    }
}

impl std::fmt::Debug for CancellableLLMProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CancellableLLMProvider")
            .field("provider", &self.inner.name())
            .field("deadline", &self.guard.deadline())
            .finish()
    }
}

#[async_trait]
impl LLMProvider for CancellableLLMProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        self.guard.admit()?;
        // The connection handshake is itself cancellable: a provider that hangs
        // before returning a stream must not outlive the scope either.
        let stream = self.guard.run(self.inner.complete(request)).await??;
        Ok(guarded_stream(stream, self.guard.clone()))
    }

    async fn complete_shared(&self, request: SharedCompletionRequest) -> Result<CompletionStream> {
        self.guard.admit()?;
        // The shared request remains immutable through the cancellation layer;
        // the inner provider owns any provider-specific transformation.
        let stream = self
            .guard
            .run(self.inner.complete_shared(request))
            .await??;
        Ok(guarded_stream(stream, self.guard.clone()))
    }
}

/// Wraps a completion stream so the scope terminates it mid-flight.
///
/// Dropping the returned stream aborts the forwarding task, which drops the
/// inner stream, whose own `Drop` aborts the provider task. The chain is what
/// makes an expired deadline stop the upstream request rather than merely stop
/// the caller from reading it.
#[must_use]
pub fn guarded_stream(stream: CompletionStream, guard: DeadlineGuard) -> CompletionStream {
    stream.transform(FORWARD_BUFFER_BLOCKS, move |mut inner, sender| async move {
        let cancelled_or_expired = guard.cancelled_or_expired();
        tokio::pin!(cancelled_or_expired);

        let terminal_error = loop {
            let block = tokio::select! {
                // Biased so an already-dead scope wins deterministically
                // instead of forwarding one more block on a coin flip.
                biased;
                error = &mut cancelled_or_expired => break Some(error),
                block = inner.next() => block,
            };
            let Some(block) = block else {
                break None;
            };

            let sent = tokio::select! {
                // Cancellation must also interrupt output backpressure. A
                // retained consumer can fill this bounded channel without
                // polling it, and the provider must still stop on time.
                biased;
                error = &mut cancelled_or_expired => break Some(error),
                sent = sender.send(block) => sent,
            };
            if sent.is_err() {
                break None;
            }
        };

        if let Some(error) = terminal_error {
            let error = MoaError::from(error);
            // Abort the provider before attempting to notify the consumer.
            // Notification is best-effort because the bounded channel may be
            // full — the exact backpressure case cancellation must escape.
            drop(inner);
            let _ = sender.try_send(Err(clone_stream_error(&error)));
            return Err(error);
        }

        guard.run(inner.collect()).await?
    })
}

/// Reproduces a terminal stream error for the consumer channel.
///
/// [`MoaError`] is not `Clone` (it wraps `io::Error`), and the two error paths
/// out of a guarded stream carry the same fact, so the channel copy is rebuilt
/// from the message rather than the error being moved and the caller left with
/// nothing.
fn clone_stream_error(error: &MoaError) -> MoaError {
    match error {
        MoaError::Cancelled => MoaError::Cancelled,
        other => MoaError::BudgetExhausted(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration as StdDuration;

    use async_trait::async_trait;
    use chrono::{Duration, Utc};
    use moa_core::{
        error::MoaError, error::Result, traits::LLMProvider, types::completion::CompletionContent,
        types::completion::CompletionRequest, types::completion::CompletionResponse,
        types::completion::CompletionStream, types::completion::SharedCompletionRequest,
        types::completion::StopReason, types::completion::TokenUsage, types::identifiers::ModelId,
        types::model::ModelCapabilities, types::resource::DeadlineGuard,
    };
    use tokio::sync::mpsc;

    use super::{CancellableLLMProvider, FORWARD_BUFFER_BLOCKS};

    /// Counts dispatches and streams blocks on demand, so a test can observe
    /// both "was the provider called at all" and "did the in-flight call stop".
    struct RecordingProvider {
        dispatches: Arc<AtomicUsize>,
        /// Set once the streaming task has been dropped or has finished.
        producer_finished: Arc<AtomicUsize>,
        /// Blocks accepted by the provider stream channel.
        blocks_sent: Arc<AtomicUsize>,
        /// Blocks emitted before the producer parks forever.
        leading_blocks: usize,
    }

    impl RecordingProvider {
        fn new(leading_blocks: usize) -> Self {
            Self {
                dispatches: Arc::new(AtomicUsize::new(0)),
                producer_finished: Arc::new(AtomicUsize::new(0)),
                blocks_sent: Arc::new(AtomicUsize::new(0)),
                leading_blocks,
            }
        }
    }

    #[async_trait]
    impl LLMProvider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
            self.dispatches.fetch_add(1, Ordering::SeqCst);
            let leading_blocks = self.leading_blocks;
            let finished = Arc::clone(&self.producer_finished);
            let blocks_sent = Arc::clone(&self.blocks_sent);
            let (sender, receiver) = mpsc::channel(4);
            let completion = tokio::spawn(async move {
                // Marks the producer as no longer running whenever this task
                // ends, including when it is aborted by a dropped stream.
                let _sentinel = FinishSentinel(finished);
                for index in 0..leading_blocks {
                    if sender
                        .send(Ok(CompletionContent::Text(format!("block-{index}"))))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    blocks_sent.fetch_add(1, Ordering::SeqCst);
                }
                // Stands in for an upstream that keeps the connection open and
                // keeps billing: it never completes on its own.
                std::future::pending::<()>().await;
                unreachable!("the guarded stream must terminate this task")
            });
            Ok(CompletionStream::new(receiver, completion))
        }

        async fn complete_shared(
            &self,
            _request: SharedCompletionRequest,
        ) -> Result<CompletionStream> {
            self.complete(CompletionRequest::new("shared-test")).await
        }
    }

    struct FinishSentinel(Arc<AtomicUsize>);

    impl Drop for FinishSentinel {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A provider that returns a complete buffered response immediately.
    struct InstantProvider {
        dispatches: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl LLMProvider for InstantProvider {
        fn name(&self) -> &str {
            "instant"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
            self.dispatches.fetch_add(1, Ordering::SeqCst);
            Ok(CompletionStream::from_response(CompletionResponse {
                text: "done".to_string(),
                content: vec![CompletionContent::Text("done".to_string())],
                stop_reason: StopReason::EndTurn,
                model: ModelId::new("test-model"),
                usage: TokenUsage::default(),
                duration_ms: 0,
                thought_signature: None,
            }))
        }

        async fn complete_shared(
            &self,
            _request: SharedCompletionRequest,
        ) -> Result<CompletionStream> {
            self.complete(CompletionRequest::new("shared-test")).await
        }
    }

    #[tokio::test]
    async fn a_cancelled_scope_dispatches_zero_provider_calls_offline() {
        // Pins: cancellation is checked *before* the inner provider is reached,
        // so a turn cancelled between admission and dispatch cannot bill a
        // single call. Racing the call against the token in a `select!` would
        // not pin this: the request branch is still polled, and this provider
        // answers immediately.
        let dispatches = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(InstantProvider {
            dispatches: Arc::clone(&dispatches),
        });
        let guard = DeadlineGuard::unbounded();
        let provider = CancellableLLMProvider::new(inner, guard.clone());

        provider
            .complete(CompletionRequest::new("first"))
            .await
            .expect("a live scope dispatches")
            .collect()
            .await
            .expect("collect");
        assert_eq!(dispatches.load(Ordering::SeqCst), 1);

        guard.cancel();
        for _ in 0..5 {
            let error = provider
                .complete(CompletionRequest::new("after cancel"))
                .await
                .expect_err("a cancelled scope must refuse");
            assert!(matches!(error, MoaError::Cancelled), "got {error:?}");
        }
        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            1,
            "no provider call may be issued after the scope is cancelled"
        );
    }

    #[tokio::test]
    async fn an_expired_deadline_dispatches_zero_provider_calls_offline() {
        let dispatches = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(InstantProvider {
            dispatches: Arc::clone(&dispatches),
        });
        // The root guard owns the shared token; the scoped guard binds the
        // expired deadline to that same token, so `root` observes what the
        // deadline does to siblings.
        let root = DeadlineGuard::unbounded();
        let provider = CancellableLLMProvider::new(
            inner,
            DeadlineGuard::new(
                root.token().clone(),
                Some(Utc::now() - Duration::seconds(1)),
            ),
        );

        let error = provider
            .complete(CompletionRequest::new("late"))
            .await
            .expect_err("an expired scope must refuse");
        assert!(
            matches!(error, MoaError::BudgetExhausted(_)),
            "got {error:?}"
        );
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);
        assert!(
            root.is_cancelled(),
            "an expired deadline must cancel the shared token so siblings unwind"
        );
    }

    #[tokio::test]
    async fn cancelling_mid_stream_terminates_the_stream_and_stops_the_provider_task_offline() {
        // Pins: an in-flight streaming call observes cancellation. The stream
        // ends with a typed error instead of hanging on an upstream that never
        // closes, and the producer task is actually stopped rather than left
        // running detached.
        let inner = Arc::new(RecordingProvider::new(2));
        let producer_finished = Arc::clone(&inner.producer_finished);
        let guard = DeadlineGuard::unbounded();
        let provider = CancellableLLMProvider::new(inner, guard.clone());

        let mut stream = provider
            .complete(CompletionRequest::new("stream"))
            .await
            .expect("dispatch");
        assert!(matches!(
            stream.next().await,
            Some(Ok(CompletionContent::Text(_)))
        ));

        guard.cancel();
        // Drain until the guard's terminal error arrives; leading blocks that
        // were already buffered are allowed through first.
        let terminal = loop {
            match stream.next().await {
                Some(Ok(_)) => continue,
                Some(Err(error)) => break Some(error),
                None => break None,
            }
        };
        assert!(
            matches!(terminal, Some(MoaError::Cancelled)),
            "the stream must end with a cancellation, got {terminal:?}"
        );

        let error = stream
            .collect()
            .await
            .expect_err("the aggregated response must also fail");
        assert!(matches!(error, MoaError::Cancelled), "got {error:?}");

        // The producer future never completes on its own, so its sentinel can
        // only have run because the guarded stream dropped it.
        tokio::time::timeout(StdDuration::from_secs(5), async {
            while producer_finished.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the in-flight provider task must be stopped, not orphaned");
    }

    #[tokio::test]
    async fn cancellation_stops_provider_while_retained_consumer_backpressures_stream_offline() {
        // Pins: a caller may retain a completion stream without polling it. Once
        // the eight-block forwarding buffer fills, cancellation must interrupt
        // the blocked outer send and abort the real provider completion task.
        let inner = Arc::new(RecordingProvider::new(FORWARD_BUFFER_BLOCKS + 8));
        let blocks_sent = Arc::clone(&inner.blocks_sent);
        let producer_finished = Arc::clone(&inner.producer_finished);
        let guard = DeadlineGuard::unbounded();
        let provider = CancellableLLMProvider::new(inner, guard.clone());

        let stream = provider
            .complete(CompletionRequest::new("stream"))
            .await
            .expect("dispatch");

        tokio::time::timeout(StdDuration::from_secs(5), async {
            while blocks_sent.load(Ordering::SeqCst) <= FORWARD_BUFFER_BLOCKS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the retained consumer must backpressure more than eight provider blocks");
        assert_eq!(
            producer_finished.load(Ordering::SeqCst),
            0,
            "the provider must still be live before cancellation"
        );

        guard.cancel();
        tokio::time::timeout(StdDuration::from_secs(5), async {
            while producer_finished.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancellation must abort a provider blocked behind the retained consumer");
        assert_eq!(
            producer_finished.load(Ordering::SeqCst),
            1,
            "the provider completion task must stop exactly once"
        );

        drop(stream);
    }

    #[tokio::test]
    async fn a_deadline_expiring_mid_stream_terminates_the_stream_offline() {
        // Pins: the deadline branch, not just the token branch, ends an
        // in-flight stream — and cancels the shared token on the way out so a
        // sibling watching only the token also stops.
        let inner = Arc::new(RecordingProvider::new(1));
        let producer_finished = Arc::clone(&inner.producer_finished);
        let root = DeadlineGuard::unbounded();
        let guard = DeadlineGuard::new(
            root.token().clone(),
            Some(Utc::now() + Duration::milliseconds(150)),
        );
        let provider = CancellableLLMProvider::new(inner, guard);

        let mut stream = provider
            .complete(CompletionRequest::new("stream"))
            .await
            .expect("dispatch");
        assert!(matches!(
            stream.next().await,
            Some(Ok(CompletionContent::Text(_)))
        ));

        let terminal = tokio::time::timeout(StdDuration::from_secs(5), stream.next())
            .await
            .expect("the deadline must end the stream rather than leaking the task");
        assert!(
            matches!(terminal, Some(Err(MoaError::BudgetExhausted(_)))),
            "expected a deadline refusal, got {terminal:?}"
        );
        assert!(root.is_cancelled());

        tokio::time::timeout(StdDuration::from_secs(5), async {
            while producer_finished.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the in-flight provider task must be stopped, not orphaned");
    }

    #[tokio::test]
    async fn a_live_scope_forwards_every_block_and_the_final_response_offline() {
        // Pins: the guard is transparent when nothing fires. Without this, a
        // guard that dropped blocks or swallowed the aggregated response would
        // still pass every cancellation test above.
        let inner = Arc::new(InstantProvider {
            dispatches: Arc::new(AtomicUsize::new(0)),
        });
        let provider = CancellableLLMProvider::new(
            inner,
            DeadlineGuard::new(
                DeadlineGuard::unbounded().token().clone(),
                Some(Utc::now() + Duration::hours(1)),
            ),
        );

        let mut stream = provider
            .complete(CompletionRequest::new("hello"))
            .await
            .expect("dispatch");
        let mut blocks = Vec::new();
        while let Some(block) = stream.next().await {
            blocks.push(block.expect("no error on a live scope"));
        }
        assert_eq!(blocks.len(), 1);

        let response = stream.collect().await.expect("aggregated response");
        assert_eq!(response.text, "done");
        assert_eq!(response.stop_reason, StopReason::EndTurn);
    }
}
