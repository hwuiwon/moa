//! Durable and process-local Slack message reference tracking.

use std::{sync::Arc, time::Duration, time::Instant};

use moa_core::traits::RuntimeCacheStore;
use moa_core::{
    error::MoaError, error::Result, types::channel::ChannelRef, types::channel::MessageId,
};
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::warn;
use uuid::Uuid;

const SLACK_MESSAGE_ID_PREFIX: &str = "slack:";
const SLACK_OUTBOUND_REF_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const SLACK_OUTBOUND_REF_LOCK_TTL: Duration = Duration::from_secs(120);
pub(super) const SLACK_OUTBOUND_REF_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(25);
const SLACK_OUTBOUND_REF_LOCK_RELEASE_TTL: Duration = Duration::from_millis(1);
/// Deadline for acquiring the shared outbound-ref update lock before giving up.
///
/// A crashed holder's lock auto-expires after [`SLACK_OUTBOUND_REF_LOCK_TTL`], so
/// waiting one lock TTL guarantees acquisition after a dead holder while bounding
/// how long an edit/delete can block on a live contender.
const SLACK_OUTBOUND_REF_LOCK_ACQUIRE_TIMEOUT: Duration = SLACK_OUTBOUND_REF_LOCK_TTL;
/// Upper bound on process-local outbound reference entries.
const SLACK_OUTBOUND_REF_CACHE_MAX_CAPACITY: u64 = 100_000;

#[derive(Clone)]
pub(super) struct SlackOutboundMessageRefs {
    hot_refs: Cache<String, Vec<SlackMessageRef>>,
    pub(super) runtime_cache: Option<Arc<dyn RuntimeCacheStore>>,
}

pub(super) struct SlackOutboundRefUpdateLock {
    runtime_cache: Arc<dyn RuntimeCacheStore>,
    key: String,
    token: Vec<u8>,
}

impl SlackOutboundRefUpdateLock {
    async fn release(self) -> Result<()> {
        self.runtime_cache
            .compare_and_set(
                &self.key,
                Some(&self.token),
                b"released".to_vec(),
                SLACK_OUTBOUND_REF_LOCK_RELEASE_TTL,
            )
            .await?;
        Ok(())
    }
}

impl SlackOutboundMessageRefs {
    pub(super) fn new(runtime_cache: Option<Arc<dyn RuntimeCacheStore>>) -> Self {
        Self {
            hot_refs: Cache::builder()
                .max_capacity(SLACK_OUTBOUND_REF_CACHE_MAX_CAPACITY)
                .time_to_live(SLACK_OUTBOUND_REF_CACHE_TTL)
                .build(),
            runtime_cache,
        }
    }

    pub(super) async fn resolve(&self, msg_id: &MessageId) -> Result<Vec<SlackMessageRef>> {
        if let Some(refs) = self.load(msg_id).await? {
            return Ok(refs);
        }

        let Some(message_ref) = slack_message_ref_from_id(msg_id) else {
            return Err(MoaError::ValidationError(format!(
                "unknown slack message id: {msg_id}"
            )));
        };
        let refs = vec![message_ref];
        self.refresh_shared_refs_best_effort(msg_id, &refs, "resolve_single_ref")
            .await;
        Ok(refs)
    }

    #[cfg(test)]
    pub(super) async fn store(&self, msg_id: &MessageId, refs: Vec<SlackMessageRef>) -> Result<()> {
        self.remember_hot_refs(msg_id, &refs).await;
        self.write_shared_refs(msg_id, &refs).await?;

        Ok(())
    }

    pub(super) async fn record_after_external_side_effect(
        &self,
        msg_id: &MessageId,
        refs: Vec<SlackMessageRef>,
        operation: &'static str,
    ) {
        self.remember_hot_refs(msg_id, &refs).await;
        self.refresh_shared_refs_best_effort(msg_id, &refs, operation)
            .await;
    }

    pub(super) async fn load(&self, msg_id: &MessageId) -> Result<Option<Vec<SlackMessageRef>>> {
        if let Some(runtime_cache) = &self.runtime_cache {
            let key = slack_outbound_refs_cache_key(msg_id);
            match runtime_cache.get(&key).await {
                Ok(Some(value)) => {
                    let refs = decode_slack_message_refs(&value)?;
                    self.remember_hot_refs(msg_id, &refs).await;
                    if let Err(error) = runtime_cache
                        .expire(&key, SLACK_OUTBOUND_REF_CACHE_TTL)
                        .await
                    {
                        warn!(
                            message_id = %msg_id,
                            error = %error,
                            "slack outbound refs loaded but shared TTL refresh failed"
                        );
                    }
                    return Ok(Some(refs));
                }
                Ok(None) => {}
                Err(error) => {
                    if let Some(refs) = self.hot_refs.get(msg_id.as_str()).await {
                        warn!(
                            message_id = %msg_id,
                            error = %error,
                            "using process-local Slack outbound refs after shared cache read failed"
                        );
                        return Ok(Some(refs));
                    }
                    return Err(error);
                }
            }
        }

        let refs = self.hot_refs.get(msg_id.as_str()).await;
        if let Some(refs) = refs {
            self.refresh_shared_refs_best_effort(msg_id, &refs, "load_hot_refs")
                .await;
            return Ok(Some(refs));
        }

        Ok(None)
    }

    pub(super) async fn remove_after_external_side_effect(
        &self,
        msg_id: &MessageId,
        operation: &'static str,
    ) {
        self.hot_refs.invalidate(msg_id.as_str()).await;
        if let Some(runtime_cache) = &self.runtime_cache
            && let Err(error) = runtime_cache
                .delete(&slack_outbound_refs_cache_key(msg_id))
                .await
        {
            warn!(
                message_id = %msg_id,
                operation,
                error = %error,
                "Slack accepted external side effect but shared outbound ref cleanup failed"
            );
        }
    }

    pub(super) async fn acquire_update_lock(
        &self,
        msg_id: &MessageId,
    ) -> Result<Option<SlackOutboundRefUpdateLock>> {
        let Some(runtime_cache) = &self.runtime_cache else {
            return Ok(None);
        };
        let key = slack_outbound_refs_lock_key(msg_id);
        let token = Uuid::now_v7().to_string().into_bytes();
        // The lock is held across the full edit/delete Slack API sequence to
        // serialize concurrent ref mutations for one message, so its scope is
        // kept as-is; only acquisition is bounded so an edit/delete cannot block
        // forever on a live contender or a lost lock release.
        let deadline = Instant::now() + SLACK_OUTBOUND_REF_LOCK_ACQUIRE_TIMEOUT;

        loop {
            let current = runtime_cache.get(&key).await?;
            if current.is_none()
                && runtime_cache
                    .compare_and_set(&key, None, token.clone(), SLACK_OUTBOUND_REF_LOCK_TTL)
                    .await?
            {
                return Ok(Some(SlackOutboundRefUpdateLock {
                    runtime_cache: runtime_cache.clone(),
                    key,
                    token,
                }));
            }

            if Instant::now() >= deadline {
                return Err(MoaError::ProviderQuirk(format!(
                    "timed out acquiring Slack outbound ref update lock for {msg_id}"
                )));
            }
            sleep(crate::rate_limit::with_jitter(
                SLACK_OUTBOUND_REF_LOCK_RETRY_INTERVAL,
            ))
            .await;
        }
    }

    pub(super) async fn release_update_lock(
        &self,
        update_lock: Option<SlackOutboundRefUpdateLock>,
    ) {
        let Some(update_lock) = update_lock else {
            return;
        };
        if let Err(error) = update_lock.release().await {
            warn!(
                error = %error,
                "Slack outbound ref update lock release failed"
            );
        }
    }

    async fn remember_hot_refs(&self, msg_id: &MessageId, refs: &[SlackMessageRef]) {
        self.hot_refs
            .insert(msg_id.as_str().to_string(), refs.to_vec())
            .await;
    }

    async fn write_shared_refs(&self, msg_id: &MessageId, refs: &[SlackMessageRef]) -> Result<()> {
        let Some(runtime_cache) = &self.runtime_cache else {
            return Ok(());
        };
        runtime_cache
            .set(
                &slack_outbound_refs_cache_key(msg_id),
                encode_slack_message_refs(refs)?,
                SLACK_OUTBOUND_REF_CACHE_TTL,
            )
            .await?;
        Ok(())
    }

    async fn refresh_shared_refs_best_effort(
        &self,
        msg_id: &MessageId,
        refs: &[SlackMessageRef],
        operation: &'static str,
    ) {
        if let Err(error) = self.write_shared_refs(msg_id, refs).await {
            warn!(
                message_id = %msg_id,
                operation,
                error = %error,
                "Slack outbound ref storage failed; using process-local refs when available"
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SlackTarget {
    pub(super) channel_id: Arc<str>,
    pub(super) thread_ts: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SlackMessageRef {
    pub(super) channel_id: Arc<str>,
    pub(super) ts: String,
    pub(super) thread_ts: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SlackMessageRefRecord {
    channel_id: String,
    ts: String,
    thread_ts: Option<String>,
}

impl SlackMessageRef {
    pub(super) fn target(&self) -> SlackTarget {
        SlackTarget {
            channel_id: self.channel_id.clone(),
            thread_ts: Some(self.thread_anchor().to_string()),
        }
    }

    pub(super) fn thread_anchor(&self) -> &str {
        self.thread_ts.as_deref().unwrap_or(self.ts.as_str())
    }
}

impl From<&SlackMessageRef> for SlackMessageRefRecord {
    fn from(message_ref: &SlackMessageRef) -> Self {
        Self {
            channel_id: message_ref.channel_id.to_string(),
            ts: message_ref.ts.clone(),
            thread_ts: message_ref.thread_ts.clone(),
        }
    }
}

impl From<SlackMessageRefRecord> for SlackMessageRef {
    fn from(record: SlackMessageRefRecord) -> Self {
        Self {
            channel_id: Arc::<str>::from(record.channel_id),
            ts: record.ts,
            thread_ts: record.thread_ts,
        }
    }
}

fn encode_slack_message_refs(refs: &[SlackMessageRef]) -> Result<Vec<u8>> {
    let records: Vec<SlackMessageRefRecord> =
        refs.iter().map(SlackMessageRefRecord::from).collect();
    Ok(serde_json::to_vec(&records)?)
}

fn decode_slack_message_refs(value: &[u8]) -> Result<Vec<SlackMessageRef>> {
    let records: Vec<SlackMessageRefRecord> = serde_json::from_slice(value)?;
    Ok(records.into_iter().map(SlackMessageRef::from).collect())
}

pub(super) fn slack_outbound_refs_cache_key(message_id: &MessageId) -> String {
    format!("moa:messaging:slack:outbound_refs:{message_id}")
}

fn slack_outbound_refs_lock_key(message_id: &MessageId) -> String {
    format!("moa:messaging:slack:outbound_refs_lock:{message_id}")
}

pub(super) fn slack_message_id_from_ref(message_ref: &SlackMessageRef) -> MessageId {
    MessageId::new(format!(
        "{SLACK_MESSAGE_ID_PREFIX}{}:{}",
        message_ref.channel_id, message_ref.ts
    ))
}

pub(super) fn slack_message_ref_from_id(message_id: &MessageId) -> Option<SlackMessageRef> {
    let value = message_id.as_str().strip_prefix(SLACK_MESSAGE_ID_PREFIX)?;
    let (channel_id, ts) = value.split_once(':')?;
    if channel_id.is_empty() || ts.is_empty() {
        return None;
    }
    Some(SlackMessageRef {
        channel_id: Arc::<str>::from(channel_id),
        ts: ts.to_string(),
        thread_ts: None,
    })
}

pub(super) fn slack_target_from_channel_ref(channel_ref: &ChannelRef) -> Option<SlackTarget> {
    let ChannelRef::Slack {
        slack_channel_id,
        thread_ts,
        ..
    } = channel_ref
    else {
        return None;
    };
    Some(SlackTarget {
        channel_id: Arc::<str>::from(slack_channel_id.as_ref()?.as_str()),
        thread_ts: thread_ts.clone(),
    })
}
