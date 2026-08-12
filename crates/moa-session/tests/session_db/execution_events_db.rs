//! PostgreSQL replay contracts for compact execution delivery events.

use moa_core::{
    events::{
        Event, ExecutionFailureDisposition, ExecutionInputRequired, ExecutionProgress,
        ExecutionRunEvidenceRef, ExecutionSynthesisRequested, ExecutionTaskResultsRef,
        ExecutionTerminalSummary,
    },
    traits::SessionStore,
    types::{
        contact::SessionActorRef,
        events_stream::EventRange,
        identifiers::{ModelId, TenantId},
        session::SessionMeta,
    },
};
use moa_test_support::postgres::bootstrap_test_db;
use uuid::Uuid;

#[tokio::test]
async fn execution_events_db_round_trip_compact_payloads_without_task_output_copy() {
    // Pins: replay stores only compact execution evidence and the typed task-table reference.
    let test_db = bootstrap_test_db()
        .await
        .expect("bootstrap compact execution event database");
    let session_id = test_db
        .store()
        .create_session(SessionMeta {
            tenant_id: TenantId::new(),
            created_by: Some(SessionActorRef::Identity {
                id: Uuid::from_u128(71),
            }),
            model: ModelId::new("execution-event-test"),
            ..SessionMeta::default()
        })
        .await
        .expect("create compact execution event session");
    let run_uid = Uuid::from_u128(72);
    let terminal = ExecutionTerminalSummary {
        run_uid,
        originating_user_sequence_num: 4,
        output: Some(serde_json::json!({ "aggregate": "bounded" })),
        output_hash: [3; 32],
        citation_ids: vec!["source-a".to_string()],
        failures: vec!["one bounded failure".to_string()],
        gaps: vec!["one bounded gap".to_string()],
        task_results: ExecutionTaskResultsRef::ExecutionTaskTable { run_uid },
    };
    let events = [
        Event::ExecutionProgress(ExecutionProgress {
            run_uid,
            originating_user_sequence_num: 4,
            plan_revision: 2,
            status: "running".to_string(),
            phase: moa_core::events::ExecutionProgressPhase::Running,
            waiting_since: None,
            next_wake_at: None,
            last_progress_at: chrono::Utc::now(),
            external_job_uid: None,
            ready_tasks: 2,
            active_tasks: 1,
            parked_tasks: 0,
            blocker_audience: None,
            remaining_budget: moa_core::events::ExecutionRemainingBudget {
                cost_microusd: Some(100),
                tokens: Some(1_000),
                tasks: Some(3),
                tool_calls: Some(6),
                retrieved_bytes: Some(10_000),
                deadline_at: None,
            },
            total: 6,
            completed: 3,
            failed: 1,
            cancelled: 0,
        }),
        Event::ExecutionInputRequired(ExecutionInputRequired {
            run_uid,
            originating_user_sequence_num: 4,
            task_id: Uuid::from_u128(73),
            generation: 2,
            question: "Choose a source".to_string(),
        }),
        Event::ExecutionCompleted(terminal.clone()),
        Event::ExecutionFailed {
            disposition: ExecutionFailureDisposition::Partial,
            summary: terminal.clone(),
        },
        Event::ExecutionCancelled(terminal.clone()),
        Event::ExecutionSynthesisRequested(ExecutionSynthesisRequested {
            run_uid,
            originating_user_sequence_num: 4,
            turn_id: "execution-synthesis-72-4".to_string(),
            terminal,
            run_evidence: ExecutionRunEvidenceRef::ExecutionRun { run_uid },
        }),
    ];

    for (index, event) in events.iter().cloned().enumerate() {
        let record = test_db
            .store()
            .emit_event_record(
                session_id,
                event.clone(),
                Some(format!("execution-events-db:{index}")),
            )
            .await
            .expect("persist compact execution event");
        assert_eq!(record.event, event);
    }

    let loaded = test_db
        .store()
        .get_events(session_id, EventRange::all())
        .await
        .expect("replay compact execution events");
    assert_eq!(
        loaded
            .iter()
            .map(|record| &record.event)
            .collect::<Vec<_>>(),
        events.iter().collect::<Vec<_>>()
    );
    let encoded = serde_json::to_string(&loaded).expect("serialize replayed execution events");
    assert!(encoded.contains("execution_task_table"));
    assert!(!encoded.contains("complete-task-output-sentinel"));
    assert!(!encoded.contains("__moa_blob_ref"));
}
