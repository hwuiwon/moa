//! Integration tests for the prompt-injection circuit Detection Finding.
//!
//! These drive `emit_prompt_injection_finding` against a migrated
//! `security_events` schema and pin the properties an audit trail is worthless
//! without: one deterministic identity per transition, a byte-identical replay
//! rather than a duplicate row, HMAC verification against the key the row was
//! *signed* with (not whichever key is active now), and a hard conflict when two
//! genuinely different transitions collide on one identity.

use chrono::{DateTime, TimeZone, Utc};
use moa_core::types::identifiers::{SessionId, TenantId, ToolCallId};
use moa_core::types::security::{
    InjectionSignal, OutputAssessmentClass, PROMPT_INJECTION_DETECTOR_REVISION,
    SecurityCircuitOwner, SecurityCircuitStage, SecurityCircuitTransition, ToolCapabilityId,
    TransitionKeyInput, transition_key,
};
use moa_ocsf::{FindingWrite, PromptInjectionFinding, emit_prompt_injection_finding, signing};
use sqlx::PgPool;
use uuid::Uuid;

use super::support;

/// OCSF constants the finding emitter must persist.
const DETECTION_FINDING: i32 = 2004;
const FINDINGS_CATEGORY: i32 = 2;
const CREATE_ACTIVITY: i32 = 1;
const CRITICAL: i32 = 5;
const HIGH: i32 = 4;
const MEDIUM: i32 = 3;
const LOW: i32 = 2;

/// Fixed safe finding title and description.
///
/// Duplicated from the emitter deliberately: a test that read these from the
/// code under test could not detect the day one of them starts being derived
/// from tool output.
const EXPECTED_TITLE: &str = "Prompt-injection security circuit transition";
const EXPECTED_DESC: &str = "A tool output was classified as a prompt-injection or restricted-material result and \
     advanced the owning agent's security circuit to a new stage.";

/// Fixed occurrence time so the signed payload is byte-stable across runs.
///
/// Deliberately not `Utc::now()`: the emitter must stamp the caller's journaled
/// timestamp, and a test clock read would hide a regression where it stamps its
/// own.
fn occurred_at() -> DateTime<Utc> {
    Utc.timestamp_opt(1_760_000_000, 0)
        .single()
        .expect("fixed test timestamp")
}

fn owner(generation: u64) -> SecurityCircuitOwner {
    SecurityCircuitOwner::Coordinator {
        turn_id: "turn-security-1".to_string(),
        generation,
    }
}

/// Builds a transition with a genuinely derived key, as production does.
fn transition(
    session_id: SessionId,
    generation: u64,
    reached: SecurityCircuitStage,
    tool_call_id: ToolCallId,
) -> SecurityCircuitTransition {
    let owner = owner(generation);
    let capability = ToolCapabilityId::mcp("external-search", "query");
    let key = transition_key(TransitionKeyInput {
        session_id,
        owner: &owner,
        capability: &capability,
        tool_call_id,
        prior_stage: SecurityCircuitStage::Clear,
        reached_stage: reached,
    });
    SecurityCircuitTransition {
        owner,
        capability,
        tool_call_id,
        class: OutputAssessmentClass::CanaryLeak,
        detector_revision: PROMPT_INJECTION_DETECTOR_REVISION.to_string(),
        prior_stage: SecurityCircuitStage::Clear,
        reached_stage: reached,
        prior_score: 0,
        reached_score: 4,
        key,
    }
}

fn finding(session_id: SessionId, transition: SecurityCircuitTransition) -> PromptInjectionFinding {
    PromptInjectionFinding {
        session_id: session_id.0,
        transition,
        signals: vec![InjectionSignal::CanaryToken],
        occurred_at: occurred_at(),
    }
}

/// Row shape the assertions read back.
type FindingRow = (i32, i32, i32, i64, Option<String>, Vec<u8>, String, Uuid);

async fn load_finding(pool: &PgPool, event_uid: Uuid) -> FindingRow {
    sqlx::query_as(
        "SELECT class_uid, category_uid, severity_id, type_uid, target_resource_uid, \
         event_jcs, signature_hex, signing_key_id \
         FROM security_events WHERE id = $1",
    )
    .bind(event_uid)
    .fetch_one(pool)
    .await
    .expect("the finding row should exist")
}

/// Audit columns `load_finding` does not read.
///
/// A separate loader rather than extra columns on `load_finding`: several tests
/// destructure that tuple positionally with `..`, so appending to it would
/// silently re-bind their tail patterns to a different column.
type AuditColumns = (
    i32,
    Option<String>,
    Option<String>,
    Option<String>,
    DateTime<Utc>,
);

async fn load_audit_columns(pool: &PgPool, event_uid: Uuid) -> AuditColumns {
    sqlx::query_as(
        "SELECT activity_id, actor_user_uid, actor_session_uid, retrieval_operation_id, \
         occurred_at FROM security_events WHERE id = $1",
    )
    .bind(event_uid)
    .fetch_one(pool)
    .await
    .expect("the finding audit columns should exist")
}

/// Collects every string leaf under one JSON subtree, sorted.
///
/// Used to assert the finding's *content* subtrees carry nothing but closed
/// vocabulary. Checking a handful of forbidden top-level keys is a sieve: it
/// passes a `circuit.output` field, or any other nested free-text carrier. Set
/// equality over the leaves fails the moment anything new appears at all.
fn string_leaves(value: &serde_json::Value) -> Vec<String> {
    fn walk(value: &serde_json::Value, found: &mut Vec<String>) {
        match value {
            serde_json::Value::String(text) => found.push(text.clone()),
            serde_json::Value::Array(items) => items.iter().for_each(|item| walk(item, found)),
            serde_json::Value::Object(entries) => {
                entries.values().for_each(|entry| walk(entry, found));
            }
            _ => {}
        }
    }
    let mut found = Vec::new();
    walk(value, &mut found);
    found.sort();
    found
}

#[tokio::test]
async fn prompt_injection_finding_persists_a_signed_content_free_detection_finding_db() {
    // Pins: the shape a SIEM receives. Class 2004 under the Findings category,
    // deterministic severity from the reached stage, the transition key as
    // `finding_info.uid`, the capability as the target resource, and a payload
    // that carries no output bytes at all.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let transition = transition(
        session_id,
        3,
        SecurityCircuitStage::Halted,
        ToolCallId::new(),
    );
    let expected_key = transition.key.clone();

    let (event_uid, write) =
        emit_prompt_injection_finding(&pool, tenant_id, finding(session_id, transition.clone()))
            .await
            .expect("the finding should persist");

    assert_eq!(write, FindingWrite::Inserted);
    assert_eq!(
        event_uid,
        transition.event_uuid(),
        "identity must be derived from the transition, not generated"
    );

    let (class_uid, category_uid, severity_id, type_uid, target, event_jcs, signature, key_id) =
        load_finding(&pool, event_uid).await;
    assert_eq!(class_uid, DETECTION_FINDING);
    assert_eq!(category_uid, FINDINGS_CATEGORY);
    assert_eq!(
        severity_id, CRITICAL,
        "a halt is the top deterministic severity"
    );
    assert_eq!(
        type_uid,
        i64::from(DETECTION_FINDING * 100 + CREATE_ACTIVITY)
    );
    assert_eq!(target.as_deref(), Some("mcp:external-search:query"));

    let payload: serde_json::Value =
        serde_json::from_slice(&event_jcs).expect("the payload should be canonical JSON");
    assert_eq!(
        payload
            .pointer("/finding_info/uid")
            .and_then(serde_json::Value::as_str),
        Some(expected_key.as_str()),
        "finding_info.uid must be the replay-stable transition key"
    );
    assert_eq!(
        payload
            .pointer("/circuit/reached_stage")
            .and_then(serde_json::Value::as_str),
        Some("halted")
    );
    assert_eq!(
        payload
            .pointer("/circuit/assessment_class")
            .and_then(serde_json::Value::as_str),
        Some("canary_leak")
    );
    for forbidden in ["output", "content", "structured", "stdout", "stderr"] {
        assert!(
            payload.get(forbidden).is_none(),
            "a shipped finding must carry no output carrier, found {forbidden}"
        );
    }
    assert_eq!(
        payload
            .pointer("/finding_info/title")
            .and_then(serde_json::Value::as_str),
        Some(EXPECTED_TITLE),
        "the title must stay fixed; deriving it from output would leak the attack text"
    );
    assert_eq!(
        payload
            .pointer("/finding_info/desc")
            .and_then(serde_json::Value::as_str),
        Some(EXPECTED_DESC),
        "the description must stay fixed for the same reason as the title"
    );

    // Exhaustive, not a sieve: every string the finding's content subtrees carry
    // must be closed vocabulary or an identifier MOA minted itself.
    let mut expected: Vec<String> = [
        expected_key.clone(),
        EXPECTED_TITLE.to_string(),
        EXPECTED_DESC.to_string(),
        PROMPT_INJECTION_DETECTOR_REVISION.to_string(),
        "coordinator".to_string(),
        "mcp:external-search:query".to_string(),
        transition.tool_call_id.0.to_string(),
        "canary_leak".to_string(),
        PROMPT_INJECTION_DETECTOR_REVISION.to_string(),
        "clear".to_string(),
        "halted".to_string(),
        "canary_token".to_string(),
    ]
    .to_vec();
    expected.sort();
    let mut actual = string_leaves(&payload["finding_info"]);
    actual.extend(string_leaves(&payload["circuit"]));
    actual.sort();
    assert_eq!(
        actual, expected,
        "the finding's content subtrees must carry only closed vocabulary and MOA-minted \
         identifiers; any new free-text field here is a channel for attacker bytes"
    );

    let (activity_id, actor_user_uid, actor_session_uid, retrieval_operation_id, stored_time) =
        load_audit_columns(&pool, event_uid).await;
    assert_eq!(activity_id, CREATE_ACTIVITY);
    assert_eq!(
        actor_user_uid, None,
        "a circuit transition has no human actor; naming one would be a lie"
    );
    assert_eq!(
        actor_session_uid.as_deref(),
        Some(session_id.0.to_string().as_str()),
        "the owning session must be queryable"
    );
    assert_eq!(
        retrieval_operation_id, None,
        "circuit findings must never borrow the data-access retrieval idempotency \
         contract; populating it would collide two unrelated uniqueness rules"
    );
    assert_eq!(
        stored_time,
        occurred_at(),
        "the owner's journaled timestamp is persisted verbatim, never re-read here"
    );

    assert!(
        signing::verify(&pool, key_id, &event_jcs, &signature)
            .await
            .expect("verification should run"),
        "the persisted finding must verify under its own signing key"
    );
}

#[tokio::test]
async fn replaying_one_transition_matches_the_existing_finding_instead_of_duplicating_db() {
    // Pins: a crashed-and-replayed owner writes one row. Identity is UUIDv5 over
    // the transition key, so the second attempt collides on the primary key and
    // must be recognized as the same finding rather than inserted again or
    // reported as drift.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let transition = transition(
        session_id,
        3,
        SecurityCircuitStage::Halted,
        ToolCallId::new(),
    );

    let (first_uid, first_write) =
        emit_prompt_injection_finding(&pool, tenant_id, finding(session_id, transition.clone()))
            .await
            .expect("the first write should insert");
    let (replay_uid, replay_write) =
        emit_prompt_injection_finding(&pool, tenant_id, finding(session_id, transition.clone()))
            .await
            .expect("an identical replay should be accepted");

    assert_eq!(first_write, FindingWrite::Inserted);
    assert_eq!(
        replay_write,
        FindingWrite::ReplayMatched,
        "the replay must be recognized, not re-inserted"
    );
    assert_eq!(first_uid, replay_uid);

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM security_events WHERE id = $1")
        .bind(first_uid)
        .fetch_one(&pool)
        .await
        .expect("count the finding rows");
    assert_eq!(rows, 1, "replay must not append a second audit row");
}

#[tokio::test]
async fn a_replay_after_key_rotation_still_verifies_against_the_stored_key_db() {
    // Pins: verification resolves the key by the row's own `signing_key_id`.
    // Checking against the tenant's *currently active* key would report drift for
    // every finding written before the latest rotation, turning routine key
    // hygiene into a flood of false audit conflicts.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let transition = transition(
        session_id,
        3,
        SecurityCircuitStage::Halted,
        ToolCallId::new(),
    );

    let (event_uid, _) =
        emit_prompt_injection_finding(&pool, tenant_id, finding(session_id, transition.clone()))
            .await
            .expect("the first write should insert");
    let (.., original_key_id) = load_finding(&pool, event_uid).await;

    let rotated_key_id = signing::rotate_key(&pool, tenant_id.0)
        .await
        .expect("rotate the tenant signing key");
    assert_ne!(
        rotated_key_id, original_key_id,
        "rotation must produce a new active key"
    );

    let (replay_uid, replay_write) =
        emit_prompt_injection_finding(&pool, tenant_id, finding(session_id, transition))
            .await
            .expect("a replay after rotation must still be accepted");

    assert_eq!(replay_uid, event_uid);
    assert_eq!(replay_write, FindingWrite::ReplayMatched);
}

#[tokio::test]
async fn a_conflicting_payload_under_one_identity_is_rejected_as_a_replay_conflict_db() {
    // Pins: identity collision is never absorbed. Two different transitions that
    // somehow share an identity mean the derivation is broken; silently accepting
    // the second would let one finding overwrite or masquerade as another.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let transition = transition(
        session_id,
        3,
        SecurityCircuitStage::Halted,
        ToolCallId::new(),
    );

    emit_prompt_injection_finding(&pool, tenant_id, finding(session_id, transition.clone()))
        .await
        .expect("the first write should insert");

    // Same key (therefore the same identity), different reached stage — the exact
    // drift the conflict check exists to catch.
    let mut conflicting = transition.clone();
    conflicting.reached_stage = SecurityCircuitStage::Disabled;
    conflicting.reached_score = 2;
    let error = emit_prompt_injection_finding(&pool, tenant_id, finding(session_id, conflicting))
        .await
        .expect_err("a conflicting payload under one identity must be rejected");

    assert!(
        matches!(error, moa_ocsf::EmitError::ReplayConflict(ref message)
            if message.contains("canonical payload")),
        "expected a canonical-payload replay conflict, got: {error:?}"
    );
}

#[tokio::test]
async fn a_conflicting_occurrence_time_under_one_identity_is_rejected_db() {
    // Pins: the timestamp is part of what a replay must reproduce. If the owner
    // stamped a fresh clock read on the second attempt, the audit trail would
    // disagree with itself about when the attack happened.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let transition = transition(
        session_id,
        3,
        SecurityCircuitStage::Halted,
        ToolCallId::new(),
    );

    emit_prompt_injection_finding(&pool, tenant_id, finding(session_id, transition.clone()))
        .await
        .expect("the first write should insert");

    let mut drifted = finding(session_id, transition);
    drifted.occurred_at = occurred_at() + chrono::Duration::seconds(1);
    let error = emit_prompt_injection_finding(&pool, tenant_id, drifted)
        .await
        .expect_err("a drifted occurrence time must be rejected");

    assert!(
        matches!(error, moa_ocsf::EmitError::ReplayConflict(ref message)
            if message.contains("occurrence time")),
        "expected an occurrence-time replay conflict, got: {error:?}"
    );
}

#[tokio::test]
async fn severity_is_derived_deterministically_from_the_reached_stage_db() {
    // Pins: severity is a pure function of the stage, so the same transition
    // always signs to the same bytes. All four reachable stages are covered —
    // an operator alerting on severity alone depends on every one of them, and
    // a half-covered map lets two of the four drift unnoticed.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let disabled = transition(
        session_id,
        3,
        SecurityCircuitStage::Disabled,
        ToolCallId::new(),
    );

    let (event_uid, _) =
        emit_prompt_injection_finding(&pool, tenant_id, finding(session_id, disabled))
            .await
            .expect("the finding should persist");
    let (_, _, severity_id, ..) = load_finding(&pool, event_uid).await;

    assert_eq!(severity_id, MEDIUM, "a capability disable is Medium");

    for (stage, expected_severity, label) in [
        (SecurityCircuitStage::Warned, LOW, "a warning is Low"),
        (
            SecurityCircuitStage::SuspendedForInput,
            HIGH,
            "a suspend for user input is High",
        ),
        (SecurityCircuitStage::Halted, CRITICAL, "a halt is Critical"),
    ] {
        let staged = transition(session_id, 3, stage, ToolCallId::new());
        let (staged_uid, _) =
            emit_prompt_injection_finding(&pool, tenant_id, finding(session_id, staged))
                .await
                .expect("the staged finding should persist");
        let (_, _, staged_severity, ..) = load_finding(&pool, staged_uid).await;
        assert_eq!(staged_severity, expected_severity, "{label}");
    }
}
