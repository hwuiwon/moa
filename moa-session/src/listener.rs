//! Postgres LISTEN/NOTIFY-backed session event streams.

use std::sync::Arc;

use moa_core::{EventRange, EventRecord, MoaError, Result, SequenceNum, SessionId, SessionStore};
use serde::Deserialize;
use sqlx::postgres::PgListener;
use tokio::sync::mpsc;

use crate::store::PostgresSessionStore;

/// Global Postgres notification channel used for system-wide observers.
pub const GLOBAL_EVENTS_CHANNEL: &str = "moa_events_all";

const LISTENER_BUFFER_CAPACITY: usize = 256;

/// Returns the stable Postgres `LISTEN/NOTIFY` channel name for one session.
///
/// Postgres channel names are limited to 63 bytes, so MOA uses a fixed prefix
/// plus the first 16 hexadecimal characters of the session UUID. This keeps the
/// name short, deterministic, and valid as an identifier without quoting.
pub fn session_channel_name(session_id: &SessionId) -> String {
    let compact = session_id.to_string().replace('-', "");
    let suffix: String = compact.chars().take(16).collect();
    format!("moa_session_{suffix}")
}

/// Live session-event stream backed by Postgres `LISTEN/NOTIFY`.
pub struct SessionEventStream {
    rx: mpsc::Receiver<EventRecord>,
}

impl SessionEventStream {
    /// Subscribes to one session's event log and replays any missing events.
    pub async fn subscribe(
        store: Arc<PostgresSessionStore>,
        session_id: SessionId,
        from_seq: Option<SequenceNum>,
    ) -> Result<Self> {
        let channel = session_channel_name(&session_id);
        let mut listener = PgListener::connect_with(store.pool())
            .await
            .map_err(|error| {
                MoaError::StorageError(format!(
                    "failed to open Postgres listener for session {session_id}: {error}"
                ))
            })?;
        listener.listen(&channel).await.map_err(|error| {
            MoaError::StorageError(format!(
                "failed to LISTEN on Postgres channel {channel}: {error}"
            ))
        })?;

        let (tx, rx) = mpsc::channel(LISTENER_BUFFER_CAPACITY);
        let initial_from_seq = from_seq.unwrap_or(0);
        tokio::spawn(run_listener_task(
            store,
            session_id,
            initial_from_seq,
            listener,
            tx,
        ));

        Ok(Self { rx })
    }

    /// Consumes the stream and returns the underlying async receiver.
    pub fn into_receiver(self) -> mpsc::Receiver<EventRecord> {
        self.rx
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct NotifyPayload {
    seq: SequenceNum,
}

async fn run_listener_task(
    store: Arc<PostgresSessionStore>,
    session_id: SessionId,
    mut next_seq: SequenceNum,
    mut listener: PgListener,
    tx: mpsc::Sender<EventRecord>,
) {
    if let Err(error) = backfill_from(store.as_ref(), &session_id, &mut next_seq, &tx).await {
        tracing::warn!(
            session_id = %session_id,
            error = %error,
            "session listener initial backfill failed"
        );
        return;
    }

    loop {
        let notification = match listener.recv().await {
            Ok(notification) => notification,
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %error,
                    "session listener waiting for reconnect"
                );
                continue;
            }
        };

        let payload = serde_json::from_str::<NotifyPayload>(notification.payload()).ok();
        if let Some(payload) = payload
            && payload.seq < next_seq
        {
            continue;
        }

        if let Err(error) = backfill_from(store.as_ref(), &session_id, &mut next_seq, &tx).await {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "session listener backfill after notify failed"
            );
            return;
        }
    }
}

async fn backfill_from(
    store: &PostgresSessionStore,
    session_id: &SessionId,
    next_seq: &mut SequenceNum,
    tx: &mpsc::Sender<EventRecord>,
) -> Result<()> {
    let records = store
        .get_events(
            session_id.clone(),
            EventRange {
                from_seq: Some(*next_seq),
                ..EventRange::all()
            },
        )
        .await?;

    for record in records {
        *next_seq = record.sequence_num.saturating_add(1);
        if tx.send(record).await.is_err() {
            return Ok(());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;
    use moa_core::{Event, SessionMeta, SessionStatus, UserId, WorkspaceId};
    use tokio::time::timeout;

    use super::*;
    use crate::testing::{cleanup_test_schema, create_isolated_test_store};

    async fn seed_session(store: &PostgresSessionStore, session_id: &SessionId) -> Result<()> {
        let now = Utc::now();
        store
            .create_session(SessionMeta {
                id: session_id.clone(),
                workspace_id: WorkspaceId::new("workspace"),
                user_id: UserId::new("user"),
                status: SessionStatus::Running,
                created_at: now,
                updated_at: now,
                ..SessionMeta::default()
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn listen_stream_receives_cross_store_events_in_order() -> Result<()> {
        let (store, database_url, schema_name) = create_isolated_test_store().await?;
        let result = async {
            let emitter = Arc::new(store.clone());
            let observer =
                Arc::new(PostgresSessionStore::new_in_schema(&database_url, &schema_name).await?);
            let session_id = SessionId::new();
            seed_session(emitter.as_ref(), &session_id).await?;

            let mut stream = SessionEventStream::subscribe(observer, session_id.clone(), None)
                .await?
                .into_receiver();

            emitter
                .emit_event(
                    session_id.clone(),
                    Event::Warning {
                        message: "first".to_string(),
                    },
                )
                .await?;
            emitter
                .emit_event(
                    session_id.clone(),
                    Event::Warning {
                        message: "second".to_string(),
                    },
                )
                .await?;

            let first = timeout(Duration::from_secs(2), stream.recv())
                .await
                .map_err(|_| {
                    MoaError::StreamError("timed out waiting for first listener event".to_string())
                })?
                .ok_or_else(|| {
                    MoaError::StreamError("listener closed before first event".to_string())
                })?;
            let second = timeout(Duration::from_secs(2), stream.recv())
                .await
                .map_err(|_| {
                    MoaError::StreamError("timed out waiting for second listener event".to_string())
                })?
                .ok_or_else(|| {
                    MoaError::StreamError("listener closed before second event".to_string())
                })?;

            assert_eq!(first.sequence_num, 0);
            assert_eq!(second.sequence_num, 1);
            Ok(())
        }
        .await;

        cleanup_test_schema(&database_url, &schema_name).await?;
        result
    }

    #[tokio::test]
    async fn listen_stream_backfills_from_last_seen_sequence() -> Result<()> {
        let (store, database_url, schema_name) = create_isolated_test_store().await?;
        let result = async {
            let emitter = Arc::new(store.clone());
            let observer =
                Arc::new(PostgresSessionStore::new_in_schema(&database_url, &schema_name).await?);
            let session_id = SessionId::new();
            seed_session(emitter.as_ref(), &session_id).await?;

            emitter
                .emit_event(
                    session_id.clone(),
                    Event::Warning {
                        message: "before-subscribe".to_string(),
                    },
                )
                .await?;
            emitter
                .emit_event(
                    session_id.clone(),
                    Event::Warning {
                        message: "still-before-subscribe".to_string(),
                    },
                )
                .await?;

            let mut stream = SessionEventStream::subscribe(observer, session_id.clone(), Some(1))
                .await?
                .into_receiver();

            let record = timeout(Duration::from_secs(2), stream.recv())
                .await
                .map_err(|_| {
                    MoaError::StreamError("timed out waiting for backfilled event".to_string())
                })?
                .ok_or_else(|| {
                    MoaError::StreamError("listener closed before backfill".to_string())
                })?;
            assert_eq!(record.sequence_num, 1);
            Ok(())
        }
        .await;

        cleanup_test_schema(&database_url, &schema_name).await?;
        result
    }

    #[tokio::test]
    async fn rolled_back_notify_is_not_observed() -> Result<()> {
        let (store, database_url, schema_name) = create_isolated_test_store().await?;
        let result = async {
            let listener_store = Arc::new(PostgresSessionStore::new_in_schema(&database_url, &schema_name).await?);
            let session_id = SessionId::new();
            seed_session(&store, &session_id).await?;

            let mut stream =
                SessionEventStream::subscribe(listener_store, session_id.clone(), Some(0)).await?
                    .into_receiver();

            let mut tx = store.pool().begin().await.map_err(|error| {
                MoaError::StorageError(format!("failed to begin rollback test transaction: {error}"))
            })?;
            let events_table = format!("\"{schema_name}\".events");
            sqlx::query(&format!(
                "INSERT INTO {events_table} (id, session_id, sequence_num, event_type, payload, timestamp, brain_id, hand_id, token_count) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"
            ))
            .bind(uuid::Uuid::now_v7())
            .bind(session_id.0)
            .bind(0_i64)
            .bind("warning")
            .bind(sqlx::types::Json(serde_json::json!({
                "type": "warning",
                "data": { "message": "rolled-back" }
            })))
            .bind(Utc::now())
            .bind(Option::<uuid::Uuid>::None)
            .bind(Option::<String>::None)
            .bind(Option::<i32>::None)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                MoaError::StorageError(format!(
                    "failed to insert rollback-test event row: {error}"
                ))
            })?;
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(session_channel_name(&session_id))
                .bind(r#"{"seq":0}"#)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    MoaError::StorageError(format!(
                        "failed to send rollback-test notify: {error}"
                    ))
                })?;
            tx.rollback().await.map_err(|error| {
                MoaError::StorageError(format!(
                    "failed to rollback rollback-test transaction: {error}"
                ))
            })?;

            let observed = timeout(Duration::from_millis(300), stream.recv()).await;
            assert!(observed.is_err(), "rolled back notify should not reach listener");
            Ok(())
        }
        .await;

        cleanup_test_schema(&database_url, &schema_name).await?;
        result
    }
}
