//! Unit tests for Slack normalization, reference tracking, errors, and chunk orchestration.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use moa_core::traits::RuntimeCacheStore;
use moa_core::{
    error::MoaError, error::Result, types::channel::Channel, types::channel::ChannelRef,
    types::channel::MessageId,
};
use moa_runtime_store::MemoryRuntimeCacheStore;
use serde_json::json;
use slack_morphism::errors::{SlackClientApiError, SlackClientError, SlackRateLimitError};
use slack_morphism::prelude::SlackPushEventCallback;
use tokio::time::timeout;
use uuid::Uuid;

use super::adapter::SlackAdapter;
use super::chunking::{SlackChunkTransport, apply_edit_tracked, send_multi_chunk_tracked};
use super::error::{
    SLACK_RATE_LIMIT_RETRIES, SlackApiFailureClass, classify_slack_client_error, slack_client_error,
};
use super::inbound::inbound_from_push_event;
use super::refs::{
    SLACK_OUTBOUND_REF_LOCK_RETRY_INTERVAL, SlackMessageRef, SlackOutboundMessageRefs, SlackTarget,
    slack_message_id_from_ref, slack_message_ref_from_id, slack_outbound_refs_cache_key,
    slack_target_from_channel_ref,
};
use crate::renderer::SlackRenderChunk;

#[derive(Debug)]
struct FailingSetRuntimeCacheStore;

#[async_trait]
impl RuntimeCacheStore for FailingSetRuntimeCacheStore {
    async fn get(&self, _key: &str) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    async fn set(&self, _key: &str, _value: Vec<u8>, _ttl: Duration) -> Result<()> {
        Err(MoaError::StorageError(
            "runtime cache set failed".to_string(),
        ))
    }

    async fn delete(&self, _key: &str) -> Result<()> {
        Err(MoaError::StorageError(
            "runtime cache delete failed".to_string(),
        ))
    }

    async fn compare_and_set(
        &self,
        _key: &str,
        _expected: Option<&[u8]>,
        _value: Vec<u8>,
        _ttl: Duration,
    ) -> Result<bool> {
        Err(MoaError::StorageError(
            "runtime cache CAS failed".to_string(),
        ))
    }

    async fn expire(&self, _key: &str, _ttl: Duration) -> Result<()> {
        Err(MoaError::StorageError(
            "runtime cache expire failed".to_string(),
        ))
    }
}

#[test]
fn parses_inbound_message_from_push_event() {
    let event: SlackPushEventCallback = serde_json::from_value(json!({
        "team_id": "T123",
        "api_app_id": "A123",
        "event": {
            "type": "message",
            "user": "U123",
            "text": "hello slack",
            "ts": "1712668800.000100",
            "channel": "D123",
            "channel_type": "im"
        },
        "event_id": "Ev123",
        "event_time": 1712668800
    }))
    .expect("slack push event should deserialize");

    let inbound = inbound_from_push_event(&event).expect("normalized slack event");
    assert_eq!(inbound.channel, Channel::Slack);
    assert_eq!(inbound.channel_msg_id, "1712668800.000100");
    assert_eq!(inbound.text, "hello slack");
    assert_eq!(
        inbound.channel_ref,
        ChannelRef::Slack {
            team_id: Some("T123".to_string()),
            slack_channel_id: Some("D123".to_string()),
            thread_ts: None,
            user_id: Some("U123".to_string())
        }
    );
}

#[test]
fn parses_inbound_message_from_app_mention_event() {
    // Pins: an `app_mention` callback flows through the AppMention branch of
    // inbound_from_push_event and normalizes to a canonical inbound message.
    let event: SlackPushEventCallback = serde_json::from_value(json!({
        "team_id": "T123",
        "api_app_id": "A123",
        "event": {
            "type": "app_mention",
            "user": "U123",
            "text": "<@U999> please summarize this",
            "ts": "1712668800.000100",
            "channel": "C123",
            "event_ts": "1712668800.000100"
        },
        "event_id": "Ev123",
        "event_time": 1712668800
    }))
    .expect("slack app_mention event should deserialize");

    let inbound = inbound_from_push_event(&event).expect("app_mention should normalize to inbound");
    assert_eq!(inbound.channel, Channel::Slack);
    assert_eq!(inbound.channel_msg_id, "1712668800.000100");
    assert_eq!(inbound.text, "<@U999> please summarize this");
    assert_eq!(inbound.actor.external_id, "U123");
    assert_eq!(inbound.actor.display_name, "<@U123>");
    assert_eq!(
        inbound.channel_ref,
        ChannelRef::Slack {
            team_id: Some("T123".to_string()),
            slack_channel_id: Some("C123".to_string()),
            thread_ts: None,
            user_id: Some("U123".to_string())
        }
    );
}

#[test]
fn message_event_with_subtype_is_filtered_to_none() {
    // Pins: a `message` event carrying a subtype (e.g. bot_message / message_changed) is
    // dropped by inbound_from_message_event's subtype filter even though it is otherwise a
    // fully-formed, normalizable message (text + user + channel all present).
    let event: SlackPushEventCallback = serde_json::from_value(json!({
        "team_id": "T123",
        "api_app_id": "A123",
        "event": {
            "type": "message",
            "subtype": "bot_message",
            "user": "U123",
            "text": "posted by a bot integration",
            "ts": "1712668800.000100",
            "channel": "C123",
            "channel_type": "channel"
        },
        "event_id": "Ev123",
        "event_time": 1712668800
    }))
    .expect("slack message event with subtype should deserialize");

    assert!(
        inbound_from_push_event(&event).is_none(),
        "message events carrying a subtype must be filtered out, not normalized"
    );
}

#[test]
fn slack_target_uses_durable_channel_ref_when_reply_anchor_is_absent() {
    // Pins: workflow-originated progress can send after process restart using the persisted route.
    let target = slack_target_from_channel_ref(&ChannelRef::Slack {
        team_id: Some("T123".to_string()),
        slack_channel_id: Some("C123".to_string()),
        thread_ts: Some("1712668800.000100".to_string()),
        user_id: Some("U123".to_string()),
    })
    .expect("slack route with channel id should resolve");

    assert_eq!(target.channel_id.as_ref(), "C123");
    assert_eq!(target.thread_ts.as_deref(), Some("1712668800.000100"));
    assert!(
        slack_target_from_channel_ref(&ChannelRef::Chat {
            conversation_id: "chat-1".to_string(),
            user_id: None,
            client_session_id: None,
        })
        .is_none()
    );
}

#[test]
fn slack_message_id_round_trips_single_message_ref() {
    // Pins: single-message Slack sends can be edited after worker restart without process-local state.
    let message_ref = SlackMessageRef {
        channel_id: Arc::<str>::from("C123"),
        ts: "1712668800.000100".to_string(),
        thread_ts: None,
    };

    let message_id = slack_message_id_from_ref(&message_ref);
    let restored =
        slack_message_ref_from_id(&message_id).expect("durable Slack message id should parse");

    assert_eq!(message_id.as_str(), "slack:C123:1712668800.000100");
    assert_eq!(restored.channel_id.as_ref(), "C123");
    assert_eq!(restored.ts, "1712668800.000100");
    assert_eq!(restored.thread_ts, None);
    assert!(slack_message_ref_from_id(&MessageId::new("other-id")).is_none());
}

#[tokio::test]
async fn slack_multi_chunk_refs_survive_adapter_instance_boundaries_with_runtime_cache() {
    // Pins: multi-chunk Slack edit/delete continuity does not depend on one adapter instance.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let cache = Arc::new(MemoryRuntimeCacheStore::new());
    let first = SlackAdapter::new("xoxb-first", "xapp-first")
        .expect("setup: first Slack adapter should construct")
        .with_runtime_cache(cache.clone());
    let second = SlackAdapter::new("xoxb-second", "xapp-second")
        .expect("setup: second Slack adapter should construct")
        .with_runtime_cache(cache);
    let message_id = MessageId::new("multi-chunk-message");
    let refs = vec![
        SlackMessageRef {
            channel_id: Arc::<str>::from("C123"),
            ts: "1712668800.000100".to_string(),
            thread_ts: Some("1712668800.000100".to_string()),
        },
        SlackMessageRef {
            channel_id: Arc::<str>::from("C123"),
            ts: "1712668801.000200".to_string(),
            thread_ts: Some("1712668800.000100".to_string()),
        },
    ];

    first
        .test_store_outbound_refs(&message_id, refs.clone())
        .await
        .expect("first adapter state should write refs to runtime cache");

    assert_eq!(
        second
            .test_resolve_outbound_refs(&message_id)
            .await
            .expect("second adapter state should load refs from runtime cache"),
        refs
    );
    assert_eq!(
        second
            .test_resolve_outbound_refs(&message_id)
            .await
            .expect("edit/delete should resolve shared refs")
            .last()
            .expect("refs should contain the last chunk")
            .target(),
        SlackTarget {
            channel_id: Arc::<str>::from("C123"),
            thread_ts: Some("1712668800.000100".to_string()),
        }
    );
    assert_eq!(
        second
            .test_resolve_outbound_refs(&message_id)
            .await
            .expect("refs should remain available until Slack delete succeeds"),
        refs
    );
    second
        .test_remove_outbound_refs_after_external_side_effect(&message_id)
        .await;
    assert!(
        second
            .test_resolve_outbound_refs(&message_id)
            .await
            .is_err(),
        "second adapter should not resolve refs after successful Slack delete cleanup"
    );
}

#[tokio::test]
async fn slack_ref_storage_failure_after_side_effect_keeps_local_refs() {
    // Pins: Slack send/edit success is not reported as failed solely because shared ref storage failed.
    let refs = SlackOutboundMessageRefs::new(Some(Arc::new(FailingSetRuntimeCacheStore)));
    let message_id = MessageId::new("multi-chunk-side-effect");
    let sent_refs = vec![
        SlackMessageRef {
            channel_id: Arc::<str>::from("C123"),
            ts: "1712668800.000100".to_string(),
            thread_ts: Some("1712668800.000100".to_string()),
        },
        SlackMessageRef {
            channel_id: Arc::<str>::from("C123"),
            ts: "1712668801.000200".to_string(),
            thread_ts: Some("1712668800.000100".to_string()),
        },
    ];

    refs.record_after_external_side_effect(&message_id, sent_refs.clone(), "chat.postMessage")
        .await;

    assert_eq!(
        refs.resolve(&message_id)
            .await
            .expect("process-local refs should remain after shared storage failure"),
        sent_refs
    );
}

#[tokio::test]
async fn slack_hot_multi_chunk_refs_refresh_shared_cache_after_ttl_loss() {
    // Pins: loading hot multi-chunk refs refreshes shared refs instead of degrading to one chunk.
    let cache = Arc::new(MemoryRuntimeCacheStore::new());
    let first = SlackOutboundMessageRefs::new(Some(cache.clone()));
    let second = SlackOutboundMessageRefs::new(Some(cache.clone()));
    let message_id = MessageId::new("multi-chunk-refresh");
    let refs = vec![
        SlackMessageRef {
            channel_id: Arc::<str>::from("C123"),
            ts: "1712668800.000100".to_string(),
            thread_ts: Some("1712668800.000100".to_string()),
        },
        SlackMessageRef {
            channel_id: Arc::<str>::from("C123"),
            ts: "1712668801.000200".to_string(),
            thread_ts: Some("1712668800.000100".to_string()),
        },
    ];

    first
        .store(&message_id, refs.clone())
        .await
        .expect("setup: refs should write to shared cache");
    cache
        .delete(&slack_outbound_refs_cache_key(&message_id))
        .await
        .expect("setup: deleting cache key simulates TTL expiration");

    assert_eq!(
        first
            .resolve(&message_id)
            .await
            .expect("hot refs should resolve after shared TTL loss"),
        refs
    );
    assert_eq!(
        second
            .resolve(&message_id)
            .await
            .expect("hot load should refresh shared cache for other adapters"),
        refs
    );
}

#[tokio::test]
async fn slack_shared_update_lock_serializes_cross_instance_ref_updates() {
    // Pins: cross-pod Slack edits cannot race shared ref updates for the same message id.
    let cache = Arc::new(MemoryRuntimeCacheStore::new());
    let first = SlackOutboundMessageRefs::new(Some(cache.clone()));
    let second = SlackOutboundMessageRefs::new(Some(cache));
    let message_id = MessageId::new("multi-chunk-lock");
    let initial_refs = vec![
        SlackMessageRef {
            channel_id: Arc::<str>::from("C123"),
            ts: "1712668800.000100".to_string(),
            thread_ts: Some("1712668800.000100".to_string()),
        },
        SlackMessageRef {
            channel_id: Arc::<str>::from("C123"),
            ts: "1712668801.000200".to_string(),
            thread_ts: Some("1712668800.000100".to_string()),
        },
    ];
    let expanded_refs = vec![
        initial_refs[0].clone(),
        initial_refs[1].clone(),
        SlackMessageRef {
            channel_id: Arc::<str>::from("C123"),
            ts: "1712668802.000300".to_string(),
            thread_ts: Some("1712668800.000100".to_string()),
        },
    ];

    first
        .store(&message_id, initial_refs)
        .await
        .expect("setup: initial refs should write to shared cache");
    let first_lock = first
        .acquire_update_lock(&message_id)
        .await
        .expect("first edit should acquire the shared ref lock");

    assert!(
        timeout(
            SLACK_OUTBOUND_REF_LOCK_RETRY_INTERVAL / 2,
            second.acquire_update_lock(&message_id)
        )
        .await
        .is_err(),
        "second edit acquired the Slack ref lock while the first edit still held it"
    );

    first
        .record_after_external_side_effect(&message_id, expanded_refs.clone(), "chat.update")
        .await;
    first.release_update_lock(first_lock).await;
    let second_lock = second
        .acquire_update_lock(&message_id)
        .await
        .expect("second edit should acquire the lock after release");
    assert_eq!(
        second
            .resolve(&message_id)
            .await
            .expect("second edit should load refs written by the first edit"),
        expanded_refs
    );
    second.release_update_lock(second_lock).await;
}

#[test]
fn slack_rate_limit_error_maps_to_typed_moa_rate_limit() {
    // Pins: Slack 429 errors stay typed after crossing the adapter boundary.
    let error = SlackClientError::RateLimitError(
        SlackRateLimitError::new()
            .with_retry_after(Duration::from_secs(5))
            .with_http_response_body("rate limited".to_string()),
    );
    let failure = classify_slack_client_error(&error);
    assert_eq!(failure.class, SlackApiFailureClass::Retryable);
    assert_eq!(failure.http_status, Some(429));
    assert_eq!(failure.retry_after, Some(Duration::from_secs(5)));

    let error = slack_client_error("chat.postMessage", error);
    assert!(matches!(
        error,
        MoaError::RateLimited {
            retries: SLACK_RATE_LIMIT_RETRIES,
            message
        } if message == "slack Web API rate limit was exceeded: rate limited"
    ));
}

#[test]
fn slack_api_errors_are_classified_before_provider_mapping() {
    // Pins: Slack ok:false errors retain retryability rather than collapsing to opaque text.
    let retryable =
        SlackClientError::ApiError(SlackClientApiError::new("internal_error".to_string()));
    let retryable_failure = classify_slack_client_error(&retryable);
    assert_eq!(retryable_failure.class, SlackApiFailureClass::Retryable);
    assert!(retryable_failure.is_retryable());
    assert!(matches!(
        slack_client_error("chat.postMessage", retryable),
        MoaError::ProviderQuirk(message)
            if message == "slack Web API returned error code internal_error"
    ));

    let permanent =
        SlackClientError::ApiError(SlackClientApiError::new("invalid_auth".to_string()));
    let permanent_failure = classify_slack_client_error(&permanent);
    assert_eq!(permanent_failure.class, SlackApiFailureClass::Permanent);
    assert!(!permanent_failure.is_retryable());
    assert!(matches!(
        slack_client_error("chat.postMessage", permanent),
        MoaError::ProviderError(message)
            if message == "slack Web API returned error code invalid_auth"
    ));
}

#[derive(Default)]
struct FakeChunkTransport {
    fail_send_at: Option<usize>,
    send_calls: std::sync::Mutex<usize>,
    sent: std::sync::Mutex<Vec<SlackMessageRef>>,
    deleted: std::sync::Mutex<Vec<String>>,
    updated: std::sync::Mutex<Vec<String>>,
    // When set, each send records how many references are already persisted
    // for the aggregate id at the moment the send is issued, proving the
    // incremental-persistence ordering.
    persistence_probe: Option<(Arc<SlackOutboundMessageRefs>, MessageId)>,
    persisted_before_send: std::sync::Mutex<Vec<usize>>,
}

impl FakeChunkTransport {
    fn new(fail_send_at: Option<usize>) -> Self {
        Self {
            fail_send_at,
            ..Default::default()
        }
    }
}

#[async_trait]
impl SlackChunkTransport for FakeChunkTransport {
    async fn send_chunk(
        &self,
        target: &SlackTarget,
        _chunk: &SlackRenderChunk,
    ) -> Result<SlackMessageRef> {
        let index = {
            let mut calls = self.send_calls.lock().expect("send_calls lock");
            let index = *calls;
            *calls += 1;
            index
        };
        if let Some((refs, message_id)) = &self.persistence_probe {
            let persisted = refs
                .load(message_id)
                .await
                .expect("probe load")
                .map_or(0, |refs| refs.len());
            self.persisted_before_send
                .lock()
                .expect("probe lock")
                .push(persisted);
        }
        if self.fail_send_at == Some(index) {
            return Err(MoaError::ProviderError(
                "simulated chunk send failure".to_string(),
            ));
        }
        let sent_ref = SlackMessageRef {
            channel_id: target.channel_id.clone(),
            ts: format!("ts-{index}"),
            thread_ts: target.thread_ts.clone(),
        };
        self.sent.lock().expect("sent lock").push(sent_ref.clone());
        Ok(sent_ref)
    }

    async fn update_chunk(
        &self,
        message_ref: &SlackMessageRef,
        _chunk: &SlackRenderChunk,
    ) -> Result<()> {
        self.updated
            .lock()
            .expect("updated lock")
            .push(message_ref.ts.clone());
        Ok(())
    }

    async fn delete_ref(&self, message_ref: &SlackMessageRef) -> Result<()> {
        self.deleted
            .lock()
            .expect("deleted lock")
            .push(message_ref.ts.clone());
        Ok(())
    }
}

fn test_chunk(text: &str) -> SlackRenderChunk {
    SlackRenderChunk {
        text: text.to_string(),
    }
}

fn test_target() -> SlackTarget {
    SlackTarget {
        channel_id: Arc::<str>::from("C123"),
        thread_ts: Some("1700000000.000100".to_string()),
    }
}

fn memory_outbound_refs() -> SlackOutboundMessageRefs {
    SlackOutboundMessageRefs::new(Some(Arc::new(MemoryRuntimeCacheStore::default())))
}

fn test_message_ref(ts: &str) -> SlackMessageRef {
    SlackMessageRef {
        channel_id: Arc::<str>::from("C123"),
        ts: ts.to_string(),
        thread_ts: Some("1700000000.000100".to_string()),
    }
}

#[tokio::test]
async fn multi_chunk_send_compensates_already_sent_chunk_on_middle_failure() {
    // Pins: a middle-chunk send failure deletes the already-sent chunks and
    // clears the partial record, so Slack never shows a visible-but-untracked
    // partial message and no stale aggregate ref remains.
    let refs = memory_outbound_refs();
    let transport = FakeChunkTransport::new(Some(1));
    let message_id = MessageId::new(Uuid::now_v7().to_string());
    let chunks = vec![test_chunk("one"), test_chunk("two"), test_chunk("three")];

    let result =
        send_multi_chunk_tracked(&transport, &refs, &test_target(), &message_id, &chunks).await;

    assert!(result.is_err(), "the middle-chunk failure should surface");
    assert_eq!(
        transport.deleted.lock().expect("deleted lock").as_slice(),
        ["ts-0"],
        "the one already-sent chunk must be compensated-deleted"
    );
    assert!(
        refs.load(&message_id).await.expect("load refs").is_none(),
        "no partial refs may remain for the aggregate id after compensation"
    );
}

#[tokio::test]
async fn multi_chunk_send_persists_each_chunk_before_the_next() {
    // Pins: each confirmed chunk reference is persisted before the next chunk is
    // sent, so an interruption never leaves a sent chunk unrecorded.
    let refs = Arc::new(memory_outbound_refs());
    let message_id = MessageId::new(Uuid::now_v7().to_string());
    let mut transport = FakeChunkTransport::new(None);
    transport.persistence_probe = Some((refs.clone(), message_id.clone()));
    let chunks = vec![test_chunk("one"), test_chunk("two"), test_chunk("three")];

    let sent = send_multi_chunk_tracked(&transport, &refs, &test_target(), &message_id, &chunks)
        .await
        .expect("all chunks send");

    assert_eq!(sent.len(), 3);
    assert_eq!(
        transport
            .persisted_before_send
            .lock()
            .expect("probe lock")
            .as_slice(),
        [0, 1, 2],
        "before sending chunk i, exactly chunks 0..i must already be persisted"
    );
    assert_eq!(
        refs.load(&message_id)
            .await
            .expect("load refs")
            .map(|refs| refs.len()),
        Some(3)
    );
}

#[tokio::test]
async fn edit_growth_persists_new_chunk_before_a_later_new_chunk_fails() {
    // Pins: when an edit grows the message, each newly-sent chunk is persisted
    // before the next send, so a later new-chunk failure still leaves the earlier
    // new chunk tracked rather than visible-but-untracked.
    let refs = memory_outbound_refs();
    let message_id = MessageId::new(Uuid::now_v7().to_string());
    let existing = vec![test_message_ref("existing-0")];
    refs.store(&message_id, existing.clone())
        .await
        .expect("seed existing refs");
    // overlap = 1 update, then two new chunks; fail the second new send.
    let transport = FakeChunkTransport::new(Some(1));
    let rendered = vec![
        test_chunk("edit-0"),
        test_chunk("new-1"),
        test_chunk("new-2"),
    ];

    let result = apply_edit_tracked(&transport, &refs, &message_id, &existing, &rendered).await;

    assert!(
        result.is_err(),
        "the second new-chunk failure should surface"
    );
    let stored = refs
        .load(&message_id)
        .await
        .expect("load refs")
        .expect("refs remain after partial growth");
    assert_eq!(
        stored.len(),
        2,
        "the overlap chunk and the first new chunk must both stay tracked"
    );
    assert_eq!(stored[0].ts, "existing-0");
    assert_eq!(stored[1].ts, "ts-0");
}

#[tokio::test]
async fn edit_growth_persists_all_new_chunks_on_success() {
    // Pins: a successful growth edit updates overlap chunks in place and tracks
    // every newly-sent chunk.
    let refs = memory_outbound_refs();
    let message_id = MessageId::new(Uuid::now_v7().to_string());
    let existing = vec![test_message_ref("existing-0")];
    refs.store(&message_id, existing.clone())
        .await
        .expect("seed existing refs");
    let transport = FakeChunkTransport::new(None);
    let rendered = vec![
        test_chunk("edit-0"),
        test_chunk("new-1"),
        test_chunk("new-2"),
    ];

    let updated = apply_edit_tracked(&transport, &refs, &message_id, &existing, &rendered)
        .await
        .expect("edit grows the message");

    assert_eq!(updated.len(), 3);
    assert_eq!(
        refs.load(&message_id)
            .await
            .expect("load refs")
            .map(|refs| refs.len()),
        Some(3)
    );
    assert_eq!(
        transport.updated.lock().expect("updated lock").as_slice(),
        ["existing-0"],
        "only the overlap chunk is edited in place"
    );
}
