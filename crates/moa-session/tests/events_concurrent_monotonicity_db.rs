//! Concurrent session event sequence invariant tests.

use std::sync::Arc;

use moa_core::{
    Event, EventRange, MoaError, ModelId, Result, SequenceNum, SessionActorRef, SessionId,
    SessionMeta, SessionStore, TenantId,
};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use proptest::prelude::*;
use proptest::test_runner::{
    Config as ProptestConfig, FileFailurePersistence, TestCaseError, TestRunner,
};
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());
const MAX_IN_FLIGHT_EMITS: usize = 64;

async fn test_db() -> TestDb {
    bootstrap_test_db().await.expect(
        "bootstrap Postgres test database; start the compose Postgres or set MOA_DATABASE_URL",
    )
}

fn tenant_id(label: &str) -> TenantId {
    let mut bytes = [0_u8; 16];
    for (index, byte) in label.bytes().enumerate() {
        bytes[index % 16] = bytes[index % 16].wrapping_mul(31).wrapping_add(byte);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    TenantId::from(Uuid::from_bytes(bytes))
}

async fn create_session(test_db: &TestDb, index: usize) -> Result<SessionId> {
    test_db
        .store()
        .create_session(SessionMeta {
            tenant_id: tenant_id(&format!("events-monotonicity-{index}")),
            created_by: Some(SessionActorRef::Identity {
                id: Uuid::from_u128(index as u128 + 1),
            }),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
}

async fn emit_in_parallel(test_db: &TestDb, operations: Vec<(SessionId, usize)>) -> Result<()> {
    let semaphore = Arc::new(Semaphore::new(MAX_IN_FLIGHT_EMITS));
    let mut tasks = Vec::with_capacity(operations.len());
    for (session_id, index) in operations {
        let store = test_db.store().clone();
        let semaphore = Arc::clone(&semaphore);
        tasks.push(tokio::spawn(async move {
            let _permit = semaphore.acquire_owned().await.map_err(|error| {
                MoaError::StorageError(format!("emit concurrency semaphore closed: {error}"))
            })?;
            store
                .emit_event(
                    session_id,
                    Event::UserMessage {
                        text: format!("message-{index}"),
                        attachments: Vec::new(),
                    },
                )
                .await
        }));
    }

    for task in tasks {
        task.await.map_err(|error| {
            MoaError::StorageError(format!("event emit task failed to join: {error}"))
        })??;
    }
    Ok(())
}

async fn assert_dense_sequences(
    test_db: &TestDb,
    sessions: &[SessionId],
    expected_len: usize,
) -> Result<()> {
    for session_id in sessions {
        let events = test_db
            .store()
            .get_events(*session_id, EventRange::all())
            .await?;
        assert_eq!(
            events.len(),
            expected_len,
            "unexpected event count for session {session_id}"
        );
        let sequence_nums: Vec<SequenceNum> = events
            .iter()
            .map(|event| {
                assert_eq!(
                    event.session_id, *session_id,
                    "event from another session appeared under {session_id}"
                );
                event.sequence_num
            })
            .collect();
        let expected: Vec<SequenceNum> = (0..expected_len as SequenceNum).collect();
        assert_eq!(
            sequence_nums, expected,
            "sequence gap for session {session_id}"
        );
    }
    Ok(())
}

fn shuffled_operations(
    sessions: &[SessionId],
    emits_per_session: usize,
    seed: u64,
) -> Vec<(SessionId, usize)> {
    let mut keyed = Vec::with_capacity(sessions.len() * emits_per_session);
    for (session_index, session_id) in sessions.iter().copied().enumerate() {
        for emit_index in 0..emits_per_session {
            let key = stable_shuffle_key(seed, session_index as u64, emit_index as u64);
            keyed.push((key, (session_id, emit_index)));
        }
    }
    keyed.sort_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, operation)| operation).collect()
}

fn stable_shuffle_key(seed: u64, session_index: u64, emit_index: u64) -> u64 {
    let mut value = seed
        ^ session_index.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ emit_index.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

async fn run_dense_sequence_case(
    test_db: &TestDb,
    session_count: usize,
    emits_per_session: usize,
    seed: u64,
) -> Result<()> {
    let mut sessions = Vec::with_capacity(session_count);
    for index in 0..session_count {
        sessions.push(create_session(test_db, index).await?);
    }
    let operations = shuffled_operations(&sessions, emits_per_session, seed);
    emit_in_parallel(test_db, operations).await?;
    assert_dense_sequences(test_db, &sessions, emits_per_session).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequence_num_is_monotonic_under_500_concurrent_emits_in_one_session() {
    let _guard = TEST_LOCK.lock().await;
    let test_db = test_db().await;
    run_dense_sequence_case(&test_db, 1, 500, 0)
        .await
        .expect("single-session sequence invariant should hold");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequence_num_is_monotonic_per_session_across_10_concurrent_sessions_with_50_emits_each() {
    let _guard = TEST_LOCK.lock().await;
    let test_db = test_db().await;
    run_dense_sequence_case(&test_db, 10, 50, 1)
        .await
        .expect("multi-session sequence invariant should hold");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proptest_arbitrary_emit_orderings_yield_dense_per_session_sequences() {
    let _guard = TEST_LOCK.lock().await;
    let test_db = test_db().await;
    let mut config = ProptestConfig::with_cases(20);
    config.failure_persistence = Some(Box::new(FileFailurePersistence::Direct(
        "crates/moa-session/proptest-regressions/events.txt",
    )));
    let mut runner = TestRunner::new(config);
    let strategy = (1_usize..=20, 1_usize..=50, any::<u64>());
    let handle = tokio::runtime::Handle::current();

    runner
        .run(&strategy, |(session_count, emits_per_session, seed)| {
            tokio::task::block_in_place(|| {
                handle
                    .block_on(async {
                        run_dense_sequence_case(&test_db, session_count, emits_per_session, seed)
                            .await
                    })
                    .map_err(|error| TestCaseError::fail(error.to_string()))
            })
        })
        .map_err(|error| TestCaseError::fail(error.to_string()))
        .expect("proptest dense per-session sequence invariant");
}
