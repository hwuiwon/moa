//! Shared broadcast receiver helpers with explicit lag handling and metrics.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::Counter;
use tokio::sync::broadcast;

use crate::{BroadcastChannel, LagPolicy, SessionId};

/// Result of receiving from a broadcast channel with lag-aware handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecvResult<T> {
    /// One message was received successfully.
    Message(T),
    /// The broadcast receiver was closed.
    Closed,
    /// Messages were dropped and the caller should continue with a gap marker.
    Gap {
        /// Number of dropped messages reported by Tokio broadcast.
        count: u64,
    },
    /// Messages were dropped and the caller should backfill from durable storage.
    BackfillRequested {
        /// Number of dropped messages reported by Tokio broadcast.
        count: u64,
    },
    /// Messages were dropped and the caller should terminate the subscription.
    AbortRequested,
}

/// Receives one broadcast message and translates lag into a typed recovery signal.
pub async fn recv_with_lag_handling<T: Clone>(
    receiver: &mut broadcast::Receiver<T>,
    channel: BroadcastChannel,
    session_id: &SessionId,
    policy: LagPolicy,
) -> RecvResult<T> {
    match receiver.recv().await {
        Ok(message) => RecvResult::Message(message),
        Err(broadcast::error::RecvError::Closed) => RecvResult::Closed,
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            record_broadcast_lag(channel, session_id, skipped);
            tracing::warn!(
                session_id = %session_id,
                channel = channel.as_str(),
                skipped_events = skipped,
                "broadcast subscriber fell behind, dropped events"
            );
            match policy {
                LagPolicy::SkipWithGap => RecvResult::Gap { count: skipped },
                LagPolicy::BackfillFromStore => RecvResult::BackfillRequested { count: skipped },
                LagPolicy::Abort => RecvResult::AbortRequested,
            }
        }
    }
}

fn record_broadcast_lag(channel: BroadcastChannel, session_id: &SessionId, dropped: u64) {
    let channel_label = channel.as_str().to_string();
    lag_counter().add(
        dropped,
        &[
            KeyValue::new("channel", channel_label.clone()),
            KeyValue::new("session_id", session_id.to_string()),
        ],
    );
    lag_counter_by_channel().add(dropped, &[KeyValue::new("channel", channel_label)]);
}

fn lag_counter() -> &'static Counter<u64> {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        global::meter("moa.broadcast")
            .u64_counter("moa_broadcast_lag_events_dropped_total")
            .with_description(
                "Number of live broadcast events dropped because a subscriber lagged behind.",
            )
            .build()
    })
}

fn lag_counter_by_channel() -> &'static Counter<u64> {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        global::meter("moa.broadcast").u64_counter("moa_broadcast_lag_events_dropped_by_channel_total")
            .with_description("Number of live broadcast events dropped because a subscriber lagged behind, aggregated without session labels.")
            .build()
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tokio::sync::broadcast;
    use uuid::Uuid;

    use super::*;
    use crate::{Event, EventRecord, EventType};

    #[tokio::test]
    async fn recv_with_lag_handling_returns_gap_for_skip_policy() {
        let (tx, mut rx) = broadcast::channel(4);
        let session_id = SessionId::new();

        for idx in 0..20 {
            let _ = tx.send(EventRecord {
                id: Uuid::now_v7(),
                session_id: session_id.clone(),
                sequence_num: idx,
                event_type: EventType::Warning,
                event: Event::Warning {
                    message: format!("event-{idx}"),
                },
                timestamp: Utc::now(),
                brain_id: None,
                hand_id: None,
                token_count: None,
            });
        }

        assert_eq!(
            recv_with_lag_handling(
                &mut rx,
                BroadcastChannel::Event,
                &session_id,
                LagPolicy::SkipWithGap,
            )
            .await,
            RecvResult::Gap { count: 16 }
        );
    }

    #[tokio::test]
    async fn recv_with_lag_handling_returns_backfill_signal() {
        let (tx, mut rx) = broadcast::channel(4);
        let session_id = SessionId::new();

        for idx in 0..20 {
            let _ = tx.send(idx);
        }

        assert_eq!(
            recv_with_lag_handling(
                &mut rx,
                BroadcastChannel::Runtime,
                &session_id,
                LagPolicy::BackfillFromStore,
            )
            .await,
            RecvResult::BackfillRequested { count: 16 }
        );
    }

    #[tokio::test]
    async fn recv_with_lag_handling_returns_abort_signal() {
        let (tx, mut rx) = broadcast::channel(1);
        let session_id = SessionId::new();

        let _ = tx.send("first");
        let _ = tx.send("second");

        assert_eq!(
            recv_with_lag_handling(
                &mut rx,
                BroadcastChannel::Runtime,
                &session_id,
                LagPolicy::Abort,
            )
            .await,
            RecvResult::AbortRequested
        );
    }
}
