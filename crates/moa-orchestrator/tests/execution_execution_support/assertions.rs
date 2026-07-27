//! Exact audit, journal, terminal, and event-order assertions shared by service scenarios.

use anyhow::{Context, Result};
use moa_core::events::Event;
use moa_core::types::completion::CompletionRequest;
use moa_core::types::events_stream::EventRecord;
use moa_core::types::execution_planning::{
    ExecutionCompileOutcome, ExecutionCompileSource, ExecutionPlannerCallKind,
    ExecutionPlannerOutcome, ExecutionPlanningAuditEnvelope, ExecutionPlanningAuditPayload,
    ExecutionRouteKind, ExecutionRouteStage, ExecutionStrategy,
};
use moa_execution::state::{ExecutionRunStatus, ExecutionTerminalCause, ExecutionTerminalEvidence};
use moa_execution::wire::ExecutionStatusResponse;
use moa_test_support::execution_audits::load_execution_planning_audits;
use serde_json::Value;

const SYNTHESIS_INSTRUCTION: &str = "Synthesize the final user response for execution run";
const AGENT_INSTRUCTION_SUFFIX: &str = "Pinned instruction skills:";

/// Stable role assigned to one scripted-provider journal request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JournalRequestRole {
    /// Ordinary Respond or Act model-loop request.
    Normal,
    /// Strict initial generated-plan request.
    InitialPlanner,
    /// Task-local bounded Agent request.
    AgentTask,
    /// Guarded terminal synthesis request.
    Synthesis,
}

/// Deserializes canonical scripted-provider journal rows into production requests.
pub(crate) fn journal_requests(values: Vec<Value>) -> Result<Vec<CompletionRequest>> {
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            serde_json::from_value(value)
                .with_context(|| format!("decode scripted request journal row {}", index + 1))
        })
        .collect()
}

/// Classifies requests by strict response format and production prompt markers.
pub(crate) fn journal_roles(requests: &[CompletionRequest]) -> Vec<JournalRequestRole> {
    requests.iter().map(journal_role).collect()
}

/// Loads unredacted planning-audit envelopes from normalized storage.
pub(crate) async fn planning_audits(
    postgres_url: &str,
    session_id: moa_core::types::identifiers::SessionId,
) -> Result<Vec<ExecutionPlanningAuditEnvelope>> {
    load_execution_planning_audits(postgres_url, session_id).await
}

/// Asserts one exact initial deterministic route and no additional route records.
pub(crate) fn assert_initial_route(
    audits: &[ExecutionPlanningAuditEnvelope],
    decision: ExecutionRouteKind,
    strategy: Option<ExecutionStrategy>,
) {
    let routes = audits
        .iter()
        .filter_map(|audit| match &audit.payload {
            ExecutionPlanningAuditPayload::Route {
                stage,
                decision,
                strategy,
                ..
            } => Some((*stage, *decision, *strategy)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        routes,
        vec![(ExecutionRouteStage::Initial, decision, strategy)],
        "unexpected strict route audit history: {audits:#?}"
    );
}

/// Asserts that the audit history contains no planner or compiler operation.
pub(crate) fn assert_no_planner_or_compile(audits: &[ExecutionPlanningAuditEnvelope]) {
    assert!(
        audits
            .iter()
            .all(|audit| matches!(audit.payload, ExecutionPlanningAuditPayload::Route { .. })),
        "non-Durable route unexpectedly planned or compiled: {audits:#?}"
    );
}

/// Asserts one accepted initial planner call followed by one accepted generated compile.
pub(crate) fn assert_generated_plan_audits(audits: &[ExecutionPlanningAuditEnvelope]) {
    assert_eq!(
        audits.len(),
        3,
        "generated admission must emit route/planner/compile"
    );
    assert!(matches!(
        &audits[1].payload,
        ExecutionPlanningAuditPayload::PlannerCall {
            call_kind: ExecutionPlannerCallKind::InitialPlan,
            call_ordinal: 0,
            run_uid: None,
            plan_revision: None,
            outcome: ExecutionPlannerOutcome::Accepted,
            candidate_hash: Some(_),
            candidate_json: Some(_),
            compiler_report: Some(_),
            ..
        }
    ));
    assert!(matches!(
        &audits[2].payload,
        ExecutionPlanningAuditPayload::Compile {
            source: ExecutionCompileSource::GeneratedPlan,
            run_uid: None,
            plan_revision: None,
            outcome: ExecutionCompileOutcome::Accepted,
            final_plan_hash: Some(_),
            ..
        }
    ));
}

/// Asserts one accepted skill-template compiler record and no planner call.
pub(crate) fn assert_skill_template_audits(audits: &[ExecutionPlanningAuditEnvelope]) {
    assert_eq!(
        audits.len(),
        2,
        "template admission must emit only route/compile"
    );
    assert!(matches!(
        &audits[1].payload,
        ExecutionPlanningAuditPayload::Compile {
            source: ExecutionCompileSource::SkillTemplate,
            run_uid: None,
            plan_revision: None,
            outcome: ExecutionCompileOutcome::Accepted,
            final_plan_hash: Some(_),
            ..
        }
    ));
}

/// Asserts a successful terminal projection with exact cause and requirement counts.
pub(crate) fn assert_completed_terminal(
    status: &ExecutionStatusResponse,
    satisfied_requirement_count: u64,
    requirement_count: u64,
) {
    assert_eq!(status.run.status, ExecutionRunStatus::Completed);
    assert!(
        status.gaps.is_empty(),
        "completed run retained gaps: {:?}",
        status.gaps
    );
    assert_eq!(
        status.run.terminal_evidence,
        Some(ExecutionTerminalEvidence {
            cause: ExecutionTerminalCause::Completion { limit_stop: None },
            satisfied_requirement_count,
            requirement_count,
        })
    );
}

/// Asserts there are no durable execution lifecycle or synthesis events.
pub(crate) fn assert_no_execution_lifecycle_events(events: &[EventRecord]) {
    let unexpected = events
        .iter()
        .filter(|record| {
            matches!(
                record.event,
                Event::ExecutionRunStarted(_)
                    | Event::ExecutionProgress(_)
                    | Event::ExecutionInputRequired(_)
                    | Event::ExecutionCompleted(_)
                    | Event::ExecutionFailed { .. }
                    | Event::ExecutionCancelled(_)
                    | Event::ExecutionSynthesisRequested(_)
            )
        })
        .collect::<Vec<_>>();
    assert!(
        unexpected.is_empty(),
        "non-Durable route emitted execution lifecycle events: {unexpected:#?}"
    );
}

/// Returns the exact count of session events matching one predicate.
pub(crate) fn event_count(events: &[EventRecord], predicate: impl Fn(&Event) -> bool) -> usize {
    events
        .iter()
        .filter(|record| predicate(&record.event))
        .count()
}

/// Returns the sequence number of the sole event matching one predicate.
pub(crate) fn sole_event_sequence(
    events: &[EventRecord],
    label: &str,
    predicate: impl Fn(&Event) -> bool,
) -> u64 {
    let matches = events
        .iter()
        .filter(|record| predicate(&record.event))
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "expected one {label} event; events: {}",
        event_summary(events)
    );
    matches[0].sequence_num
}

/// Asserts strictly increasing event sequence numbers for one semantic lifecycle.
pub(crate) fn assert_strict_event_order(order: &[(&str, u64)]) {
    for pair in order.windows(2) {
        let [(left_label, left), (right_label, right)] = pair else {
            unreachable!("windows(2) always contains two entries")
        };
        assert!(
            left < right,
            "expected {left_label} #{left} before {right_label} #{right}; order={order:?}"
        );
    }
}

/// Returns the exact final visible assistant response.
pub(crate) fn final_brain_response(events: &[EventRecord]) -> Result<&str> {
    events
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::BrainResponse { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .context("session did not persist a BrainResponse")
}

fn journal_role(request: &CompletionRequest) -> JournalRequestRole {
    let message_text = request
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    // The execution planner sends `response_format: None` and embeds the canonical candidate
    // schema in-prompt as `<response_schema>…</response_schema>` (planner candidates carry
    // free-form JSON that provider-native strict schemas cannot represent). Identify the
    // initial-planner request by that marker. `planner_request` also builds restricted amendment
    // requests, which add "Generate only a restricted plan amendment" and must stay `Normal`.
    if message_text.contains("<response_schema>")
        && !message_text.contains("Generate only a restricted plan amendment")
    {
        return JournalRequestRole::InitialPlanner;
    }
    if message_text.contains(SYNTHESIS_INSTRUCTION) {
        JournalRequestRole::Synthesis
    } else if message_text.contains(AGENT_INSTRUCTION_SUFFIX)
        && message_text.contains("Return only JSON.")
    {
        JournalRequestRole::AgentTask
    } else {
        JournalRequestRole::Normal
    }
}

fn event_summary(events: &[EventRecord]) -> String {
    events
        .iter()
        .map(|record| format!("#{} {:?}", record.sequence_num, record.event_type))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use moa_core::types::completion::{CompletionRequest, JsonResponseFormat};
    use moa_core::types::context::ContextMessage;
    use serde_json::json;

    use super::{JournalRequestRole, journal_roles};

    /// Builds a planner prompt carrying the in-prompt candidate schema marker the
    /// execution planner emits, optionally followed by a request-kind trailer.
    fn planner_prompt(trailer: &str) -> String {
        format!(
            "Plan the execution run.\n<response_schema>{{\"type\":\"object\"}}</response_schema>\n{trailer}"
        )
    }

    #[test]
    fn journal_classification_distinguishes_normal_planner_agent_and_synthesis() {
        // Pins: service assertions cannot mistake task-local, restricted-amendment, or
        // synthesis calls for root turns, and the initial-planner marker is the in-prompt
        // `<response_schema>` block rather than a provider-native strict `response_format`.
        let normal = CompletionRequest::new("What is a DAG?");

        // A strict provider-native schema alone is not a planner request: the execution
        // planner sends `response_format: None` because planner candidates carry free-form
        // JSON that a provider-native strict schema cannot represent.
        let mut strict_non_planner = CompletionRequest::new("strict but not a planner");
        strict_non_planner.response_format = Some(JsonResponseFormat::strict_json_schema(
            "generated_execution_candidate",
            "candidate",
            json!({"type": "object"}),
        ));

        let planner = CompletionRequest::new(planner_prompt(""));
        // `planner_request` reuses the same schema marker for restricted amendments, which
        // are task-local edits rather than root planning turns.
        let amendment =
            CompletionRequest::new(planner_prompt("Generate only a restricted plan amendment"));

        let mut agent = CompletionRequest::new("{}");
        agent.messages.insert(
            0,
            ContextMessage::system(
                "Investigate.\n\nPinned instruction skills:\n\nReturn only JSON.",
            ),
        );
        let synthesis = CompletionRequest::new(
            "Synthesize the final user response for execution run 00000000 from evidence",
        );

        assert_eq!(
            journal_roles(&[
                normal,
                strict_non_planner,
                planner,
                amendment,
                agent,
                synthesis,
            ]),
            vec![
                JournalRequestRole::Normal,
                JournalRequestRole::Normal,
                JournalRequestRole::InitialPlanner,
                JournalRequestRole::Normal,
                JournalRequestRole::AgentTask,
                JournalRequestRole::Synthesis,
            ]
        );
    }
}
