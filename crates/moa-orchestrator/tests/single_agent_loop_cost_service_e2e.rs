//! Deterministic single-agent (non-delegating) loop-cost measurement over the REAL Restate path.
//!
//! This test drives the common plain single-agent turn (context → model → tools
//! → repeat) with a keyed scripted provider and reconstructs a
//! [`moa_eval_core::ConversationCost`] so the exact model turns, tool calls, and
//! internal round-trips per user message are pinned with zero LLM noise.
//!
//! Run: the fixture boots Postgres + Restate + OpenFGA testcontainers and builds
//! `moa-orchestrator-bin` with `provider-overrides`. Requires Docker.
//! `cargo nextest run -p moa-orchestrator --features integration single_agent_loop_cost`.

#![cfg(feature = "integration")]

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use moa_core::{
    events::Event,
    types::{contact::ClientMessageId, identifiers::SessionId},
};
use moa_eval_core::ConversationCost;
use moa_test_support::{ConversationOptions, OrchestratorTestFixture, drive_conversation};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use tokio::time::Instant;

const RECOVERY_TIMEOUT: Duration = Duration::from_secs(45);

/// Keyed script for a plain single-agent answer: the user question maps to a direct end-turn
/// reply with no tool calls. Anything else the loop asks the model for internally (query rewrite,
/// segment assessment, …) falls through to `default`, so the measured `model_turns` reveals how
/// many model round-trips the loop really spends to answer one simple message.
fn single_answer_script() -> serde_json::Value {
    json!({
        "default": { "content": "OK" },
        "keyed": [
            {
                "match": "capital of France",
                "completion": {
                    "content": "The capital of France is Paris.",
                    "stop_reason": "end_turn"
                }
            }
        ]
    })
}

#[tokio::test]
async fn single_agent_plain_answer_loop_cost_service_e2e() {
    let fixture = OrchestratorTestFixture::with_script_and_env(
        single_answer_script(),
        vec![("MOA_PERSIST_TURN_METRICS".to_string(), "1".to_string())],
    )
    .await
    .expect("boot scripted orchestrator fixture");

    let test = fixture.isolated().await;
    let session_id = test
        .create_session("single-agent-cost")
        .await
        .expect("create + init session");

    let events = drive_conversation(
        test.client(),
        session_id,
        &["What is the capital of France?"],
        ConversationOptions::default(),
    )
    .await
    .expect("drive the single-agent conversation to completion");

    let cost = ConversationCost::from_events(&events);
    eprintln!("BASELINE single_agent_cost = {cost:#?}");

    // A plain question is answered in EXACTLY ONE model turn — the loop adds no hidden query-rewrite,
    // segment-assessment, or wrap-up model round-trips. This is the proven per-message floor; any
    // regression that inserts an extra model call for a trivial message fails here.
    assert_eq!(
        cost.model_turns, 1,
        "one plain question costs exactly one model turn"
    );
    assert_eq!(cost.total_tool_calls, 0, "a plain answer uses no tools");
    assert_eq!(cost.worker_spawns, 0, "a plain question spawns no workers");
    assert_eq!(cost.error_events, 0, "no durable errors on the happy path");
    assert_eq!(
        cost.total_input_tokens, 64,
        "one scripted turn x 64 uncached input tokens"
    );
    assert!(
        cost.total_output_tokens > 0,
        "the scripted answer carries output tokens"
    );

    // The single final reply must be present and non-empty — an empty final would be a silent
    // regression the turn/token counts alone do not catch.
    let final_text = cost
        .final_text
        .as_deref()
        .expect("the agent emitted a final BrainResponse");
    assert!(
        !final_text.is_empty(),
        "the plain-answer final reply must be non-empty"
    );
    assert_eq!(
        final_text, "The capital of France is Paris.",
        "the final reply is the scripted answer"
    );

    // The single-agent happy path makes ZERO Session/Worker VO round-trips and no fire-and-forget
    // sends — it only appends its own turn events. This is the coordination-overhead floor.
    assert!(
        cost.coordination_present,
        "TurnMetrics were persisted (MOA_PERSIST_TURN_METRICS=1)"
    );
    assert_eq!(
        cost.coordination.total_vo_calls(),
        0,
        "a non-delegating turn makes no VO round-trips"
    );
    assert_eq!(cost.coordination.vo_sends, 0, "no worker dispatch sends");
}

#[tokio::test]
#[ignore = "requires local Restate, Postgres, OpenFGA, Valkey, and dedicated crash fixture"]
async fn recovery_matrix_restart_after_llm_action_commit_records_one_response_service_e2e()
-> Result<()> {
    // Pins: a hard process crash after Restate has committed `llm_complete` but
    // before Postgres accepts BrainResponse replays the journaled response. It
    // must not redispatch the paid provider action or duplicate token/cost data.
    let fixture = OrchestratorTestFixture::with_script_and_env(
        single_answer_script(),
        vec![("MOA_PERSIST_TURN_METRICS".to_string(), "1".to_string())],
    )
    .await
    .context("boot restartable scripted orchestrator fixture")?;
    let test = fixture.isolated().await;
    let session_id = test
        .create_session("recovery-llm-action-commit")
        .await
        .context("create + initialize recovery session")?;
    let pool = PgPool::connect(&fixture.postgres_url)
        .await
        .context("connect to fixture Postgres")?;
    let gate_key = advisory_gate_key(session_id);
    install_brain_response_gate(&pool, session_id, gate_key).await?;

    let mut gate_owner = pool
        .acquire()
        .await
        .context("acquire BrainResponse advisory-gate connection")?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(gate_key)
        .execute(&mut *gate_owner)
        .await
        .context("hold BrainResponse advisory gate")?;

    let turn_id = test
        .client()
        .session(session_id.to_string())
        .start_turn(
            moa_wire::turn::StartTurnRequest {
                client_message_id: ClientMessageId::internal(
                    "recovery-matrix-llm",
                    session_id.0,
                    0,
                )
                .context("derive stable recovery message id")?,
                reply_to: None,
                stream_cursor: None,
                user_message: "What is the capital of France?".to_string(),
                attachments: Vec::new(),
                model: None,
                contact: None,
                max_turns: None,
                resource_budget: Default::default(),
                execution_template: None,
            },
            None,
        )
        .await
        .context("start recovery turn")?
        .turn_id
        .context("idle recovery session should start a turn")?;

    fixture
        .wait_for_scripted_requests(1, RECOVERY_TIMEOUT)
        .await
        .context("wait for the provider request before the crash boundary")?;
    // The workflow can reach this insert only after its awaited LLMGateway call has returned,
    // so blocking here places the crash downstream of the durably completed Restate action.
    wait_for_brain_response_gate(&pool, gate_key, RECOVERY_TIMEOUT)
        .await
        .context("wait for the post-LLM-action BrainResponse boundary")?;
    let requests_at_gate = fixture.scripted_requests()?;
    ensure!(
        requests_at_gate.len() == 2,
        "the crash boundary must follow the route classifier and one answer action, observed {}: {requests_at_gate:#?}",
        requests_at_gate.len()
    );
    let parent_invocation_id = turn_execution_invocation_id(&fixture, &turn_id.to_string()).await?;
    let action_keys = llm_action_idempotency_keys(&fixture, &parent_invocation_id).await?;
    ensure!(
        action_keys
            == vec![
                format!("moa:llm-completion:v1:{parent_invocation_id}:execution-routing:0"),
                format!("moa:llm-completion:v1:{parent_invocation_id}:root-model:1"),
            ],
        "both paid actions must be retained under their exact replay identities: {action_keys:?}"
    );

    fixture
        .hard_crash_and_restart_orchestrator()
        .await
        .context("hard-crash and restart the orchestrator at the exact LLM boundary")?;

    let requests_after_restart = fixture.scripted_requests()?;
    ensure!(
        requests_after_restart == requests_at_gate,
        "recovery must replay the committed action result without a second provider request; before={requests_at_gate:#?}, after={requests_after_restart:#?}"
    );

    let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
        .bind(gate_key)
        .fetch_one(&mut *gate_owner)
        .await
        .context("release BrainResponse advisory gate")?;
    ensure!(unlocked, "fixture connection must own the advisory gate");
    drop(gate_owner);

    wait_for_recovered_turn(&pool, session_id, RECOVERY_TIMEOUT)
        .await
        .with_context(|| format!("wait for recovered turn {turn_id} to settle"))?;
    let events = drive_conversation(
        test.client(),
        session_id,
        &[],
        ConversationOptions::default(),
    )
    .await
    .context("load the recovered durable event log")?;
    let brain_responses = events
        .iter()
        .filter(|record| matches!(record.event, Event::BrainResponse { .. }))
        .collect::<Vec<_>>();
    ensure!(
        brain_responses.len() == 1,
        "recovery must persist exactly one BrainResponse, observed {}",
        brain_responses.len()
    );
    let Event::BrainResponse {
        text,
        input_tokens_uncached,
        input_tokens_cache_write,
        input_tokens_cache_read,
        output_tokens,
        cost_cents,
        ..
    } = &brain_responses[0].event
    else {
        unreachable!("BrainResponse filter must preserve the event variant");
    };
    ensure!(text == "The capital of France is Paris.");
    ensure!(*input_tokens_uncached == 64);
    ensure!(*input_tokens_cache_write == 0);
    ensure!(*input_tokens_cache_read == 0);
    ensure!(*output_tokens > 0);
    ensure!(*cost_cents == 0, "the scripted provider has zero pricing");
    ensure!(
        brain_responses[0].token_count
            == Some(input_tokens_uncached.saturating_add(*output_tokens)),
        "the one BrainResponse row must carry the exact token record"
    );

    let cost = ConversationCost::from_events(&events);
    ensure!(cost.model_turns == 1);
    ensure!(cost.total_input_tokens == 64);
    ensure!(
        cost.total_output_tokens
            == u64::try_from(*output_tokens).context("scripted output token count fits u64")?
    );
    ensure!(cost.final_text.as_deref() == Some(text.as_str()));
    ensure!(fixture.scripted_requests()? == requests_at_gate);
    Ok(())
}

fn advisory_gate_key(session_id: SessionId) -> i64 {
    let bytes = session_id.0.as_bytes();
    i64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

async fn install_brain_response_gate(
    pool: &PgPool,
    session_id: SessionId,
    gate_key: i64,
) -> Result<()> {
    let suffix = session_id.0.simple().to_string();
    let function_name = format!("test_brain_response_gate_{}", &suffix[..16]);
    let trigger_name = format!("test_brain_response_trigger_{}", &suffix[..16]);
    let ddl = format!(
        r#"
        CREATE FUNCTION {function_name}() RETURNS trigger
        LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.session_id = '{session_id}'::uuid AND NEW.event_type = 'BrainResponse' THEN
                PERFORM pg_advisory_xact_lock({gate_key});
            END IF;
            RETURN NEW;
        END;
        $$;
        CREATE TRIGGER {trigger_name}
        BEFORE INSERT ON events
        FOR EACH ROW EXECUTE FUNCTION {function_name}();
        "#
    );
    sqlx::raw_sql(&ddl)
        .execute(pool)
        .await
        .context("install exact BrainResponse commit gate")?;
    Ok(())
}

async fn wait_for_brain_response_gate(
    pool: &PgPool,
    gate_key: i64,
    timeout: Duration,
) -> Result<i32> {
    let key_bits = gate_key as u64;
    let class_id = (key_bits >> 32) as i64;
    let object_id = (key_bits & u64::from(u32::MAX)) as i64;
    let deadline = Instant::now() + timeout;
    loop {
        let waiting: Vec<i32> = sqlx::query_scalar(
            "SELECT pid FROM pg_locks \
             WHERE locktype = 'advisory' AND NOT granted \
               AND classid::bigint = $1 AND objid::bigint = $2 \
             ORDER BY pid",
        )
        .bind(class_id)
        .bind(object_id)
        .fetch_all(pool)
        .await
        .context("inspect exact BrainResponse advisory waiter")?;
        if let [pid] = waiting.as_slice() {
            return Ok(*pid);
        }
        ensure!(
            waiting.len() <= 1,
            "expected at most one BrainResponse advisory waiter, observed {waiting:?}"
        );
        ensure!(
            Instant::now() < deadline,
            "BrainResponse insert did not reach the post-action gate within {timeout:?}; waiters={waiting:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_recovered_turn(
    pool: &PgPool,
    session_id: SessionId,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let row = sqlx::query(
            "SELECT s.status, \
                    COUNT(e.id) FILTER (WHERE e.event_type = 'BrainResponse') AS response_count \
             FROM sessions s \
             LEFT JOIN events e ON e.session_id = s.id \
             WHERE s.id = $1 \
             GROUP BY s.status",
        )
        .bind(session_id.0)
        .fetch_one(pool)
        .await
        .context("inspect recovered session and BrainResponse count")?;
        let status: String = row.try_get("status")?;
        let response_count: i64 = row.try_get("response_count")?;
        ensure!(
            response_count <= 1,
            "recovery duplicated BrainResponse before settling: {response_count}"
        );
        if status == "idle" && response_count == 1 {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "recovered session did not become idle with one BrainResponse within {timeout:?}; status={status}, response_count={response_count}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn turn_execution_invocation_id(
    fixture: &OrchestratorTestFixture,
    turn_id: &str,
) -> Result<String> {
    let rows = restate_query_rows(
        fixture,
        &format!(
            "SELECT id FROM sys_invocation WHERE target_service_name = 'TurnExecution' \
             AND target_service_key = '{turn_id}'"
        ),
    )
    .await?;
    ensure!(
        rows.len() == 1,
        "expected one TurnExecution invocation for {turn_id}, got {rows:?}"
    );
    rows[0]
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("TurnExecution introspection row omitted id")
}

async fn llm_action_idempotency_keys(
    fixture: &OrchestratorTestFixture,
    parent_invocation_id: &str,
) -> Result<Vec<String>> {
    let rows = restate_query_rows(
        fixture,
        &format!(
            "SELECT idempotency_key FROM sys_invocation WHERE invoked_by_id = \
             '{parent_invocation_id}' AND target_service_name = 'LLMGateway' \
             ORDER BY idempotency_key"
        ),
    )
    .await?;
    rows.into_iter()
        .map(|row| {
            row.get("idempotency_key")
                .and_then(Value::as_str)
                .map(str::to_string)
                .context("retained LLMGateway invocation omitted idempotency key")
        })
        .collect()
}

async fn restate_query_rows(fixture: &OrchestratorTestFixture, query: &str) -> Result<Vec<Value>> {
    let client = reqwest::Client::new();
    let url = format!("{}/query", fixture.admin_url.trim_end_matches('/'));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let last_error = match client
            .post(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&json!({"query": query}))
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                match response.text().await {
                    Ok(body) if status.is_success() => match serde_json::from_str::<Value>(&body) {
                        Ok(payload) => {
                            if let Some(rows) = payload.get("rows").and_then(Value::as_array) {
                                return Ok(rows.clone());
                            }
                            format!("response omitted rows: {payload}")
                        }
                        Err(error) => format!("decode JSON response: {error}; body={body:?}"),
                    },
                    Ok(body) => format!("status {status}; body={body:?}"),
                    Err(error) => format!("read response body: {error}"),
                }
            }
            Err(error) => format!("send query: {error}"),
        };
        ensure!(
            Instant::now() < deadline,
            "Restate LLM-action introspection did not become ready: {last_error}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
