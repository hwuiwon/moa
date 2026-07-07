//! Workflow-owned execution for the agent's procedure tools.
//!
//! `run_procedure` and `procedure_status` are declared in `moa-core` and injected
//! into a coordinator turn only when a selected skill carries a procedure. Like
//! the delegation tools, they cannot run inside the stateless `ToolExecutor`
//! because starting and polling a durable procedure run needs the Restate
//! workflow context: `run_procedure` starts the run through the same
//! `ProcedureRuntime` + `ProcedureExecution` path the `Skills` service uses, and
//! `procedure_status` reads the run projection. Execution is fire-and-poll: a
//! start returns a run id immediately and never blocks the turn waiting for a
//! terminal status, because runs can pause on review nodes for days.

use std::collections::BTreeSet;
use std::time::Duration;

use moa_artifacts::registry::ArtifactRegistry;
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::procedures::{ProcedureNodeRunSummary, ProcedureRunStatus};
use moa_core::{
    ActionRuleScope, ProcedureTool, RunProcedureToolInput, SessionActorRef, SessionId, SessionMeta,
    ToolOutput,
};
use moa_skills::procedure::error::ProcedureError;
use moa_skills::procedure::runtime::{ProcedureRuntime, StartProcedureRun};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::workflows::errors::{missing_inputs_message, procedure_handler_error};
use crate::workflows::procedure_execution::{ProcedureExecutionClient, RunProcedureRequest};

/// Executes one procedure tool call for the root session coordinator.
///
/// Returns a model-visible [`ToolOutput`]. A `run_procedure` call whose input
/// does not satisfy the procedure's `input_schema` returns a structured (non
/// error) result listing the fields to collect, so the model asks the user for
/// exactly those fields rather than treating the call as a hard failure.
///
/// `selected_procedure_skills` is the set of normalized `skill://<name>` references
/// for the procedure-capable skills selected on this turn. A `run_procedure` call is
/// only allowed to target a skill in this set; the model cannot start an arbitrary
/// visible tenant skill's procedure. `procedure_status` polls an existing run and is
/// not gated by this set (it stays tenant-scoped by [`SessionMeta::tenant_id`]).
pub(crate) async fn execute_procedure_tool(
    ctx: &WorkflowContext<'_>,
    meta: &SessionMeta,
    session_id: SessionId,
    tool: ProcedureTool,
    selected_procedure_skills: &BTreeSet<String>,
) -> Result<ToolOutput, HandlerError> {
    match tool {
        ProcedureTool::Run(input) => {
            run_procedure(ctx, meta, session_id, input, selected_procedure_skills).await
        }
        ProcedureTool::Status(status) => procedure_status(ctx, meta, &status.run_id).await,
    }
}

async fn run_procedure(
    ctx: &WorkflowContext<'_>,
    meta: &SessionMeta,
    session_id: SessionId,
    input: RunProcedureToolInput,
    selected_procedure_skills: &BTreeSet<String>,
) -> Result<ToolOutput, HandlerError> {
    // Enforce that the model can only start a procedure for a skill the context
    // pipeline selected as procedure-capable this turn. Without this, any visible
    // published tenant skill's procedure could be started via a model-supplied name.
    // The rejection returns before any run is created.
    let procedure_ref = input.procedure_ref();
    if let Some(rejection) =
        reject_unselected_procedure_skill(&procedure_ref, selected_procedure_skills)
    {
        return Ok(rejection);
    }

    let Some(identity) = session_identity(meta) else {
        return Ok(ToolOutput::error(
            "Cannot start a procedure for an anonymous session; the run must be authorized by a known session owner.",
            Duration::ZERO,
        ));
    };

    let tenant_id = meta.tenant_id;
    let scope = ActionRuleScope::Tenant { tenant_id };
    let request = StartProcedureRun {
        procedure_ref,
        input: input.input,
        session_id: Some(session_id),
        idempotency_key: input.idempotency_key,
    };

    let outcome = ctx
        .run(|| async move { start_procedure_run(scope, request).await.map(Json::from) })
        .name("run_procedure_start")
        .await?
        .into_inner();

    match outcome {
        ProcedureStartOutcome::Started { run_id, status } => {
            // Kick the durable executor exactly as the Skills service does, linking
            // the current session so the run appears in session history.
            ctx.workflow_client::<ProcedureExecutionClient>(run_id.to_string())
                .run(Json::from(RunProcedureRequest {
                    tenant_id,
                    run_uid: run_id,
                    identity,
                    session_id: Some(session_id),
                }))
                .send();
            Ok(ToolOutput::json(
                format!("Started procedure run {run_id} with status {status}."),
                serde_json::json!({
                    "run_id": run_id,
                    "status": status,
                }),
                Duration::ZERO,
            ))
        }
        ProcedureStartOutcome::MissingInputs { missing, invalid } => Ok(ToolOutput::json(
            missing_inputs_message(&missing, &invalid),
            serde_json::json!({
                "status": "missing_inputs",
                "missing_inputs": missing,
                "invalid_inputs": invalid,
            }),
            Duration::ZERO,
        )),
        ProcedureStartOutcome::Rejected { message } => {
            Ok(ToolOutput::error(message, Duration::ZERO))
        }
    }
}

async fn procedure_status(
    ctx: &WorkflowContext<'_>,
    meta: &SessionMeta,
    run_id: &str,
) -> Result<ToolOutput, HandlerError> {
    let Ok(run_uid) = Uuid::parse_str(run_id.trim()) else {
        return Ok(ToolOutput::error(
            format!("Invalid run_id `{run_id}`; use the run_id returned by run_procedure."),
            Duration::ZERO,
        ));
    };
    let scope = ActionRuleScope::Tenant {
        tenant_id: meta.tenant_id,
    };

    let status = ctx
        .run(|| async move { load_procedure_status(scope, run_uid).await.map(Json::from) })
        .name("procedure_status_read")
        .await?
        .into_inner();

    match status {
        Some(status) => {
            let summary = match status.current_node_id.as_deref() {
                Some(node) => format!(
                    "Procedure run {} is {} at node {node}.",
                    status.run_id, status.status
                ),
                None => format!("Procedure run {} is {}.", status.run_id, status.status),
            };
            Ok(ToolOutput::json(
                summary,
                serde_json::to_value(&status).unwrap_or_else(
                    |error| serde_json::json!({ "serialization_error": error.to_string() }),
                ),
                Duration::ZERO,
            ))
        }
        None => Ok(ToolOutput::error(
            format!("Procedure run {run_uid} was not found or is not visible in this tenant."),
            Duration::ZERO,
        )),
    }
}

/// Serializable outcome of a durable procedure-start step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProcedureStartOutcome {
    /// The run was created and should be kicked into durable execution.
    Started {
        /// Durable run identifier.
        run_id: Uuid,
        /// Initial run status.
        status: String,
    },
    /// The supplied input did not satisfy the procedure's `input_schema`.
    MissingInputs {
        /// Required fields absent from the supplied input.
        missing: Vec<String>,
        /// Provided fields whose value type did not match the schema.
        invalid: Vec<String>,
    },
    /// The run could not be created for a model-correctable reason.
    Rejected {
        /// Human-readable rejection reason surfaced to the model.
        message: String,
    },
}

async fn start_procedure_run(
    scope: ActionRuleScope,
    request: StartProcedureRun,
) -> Result<ProcedureStartOutcome, HandlerError> {
    match procedure_runtime().start(&scope, request).await {
        Ok(run) => Ok(ProcedureStartOutcome::Started {
            run_id: run.run_uid,
            status: run.status.as_str().to_string(),
        }),
        Err(ProcedureError::MissingRequiredInputs { missing, invalid }) => {
            Ok(ProcedureStartOutcome::MissingInputs { missing, invalid })
        }
        // Reference, visibility, and "no procedure" errors are model-correctable:
        // return them as a tool result so the agent can pick a valid skill instead
        // of failing the turn.
        Err(
            error @ (ProcedureError::InvalidReference { .. }
            | ProcedureError::WrongReferenceKind
            | ProcedureError::ProcedureNotFound { .. }
            | ProcedureError::SkillHasNoProcedure { .. }),
        ) => Ok(ProcedureStartOutcome::Rejected {
            message: error.to_string(),
        }),
        // Storage and other unexpected failures propagate so Restate can retry.
        Err(other) => Err(procedure_handler_error(other)),
    }
}

async fn load_procedure_status(
    scope: ActionRuleScope,
    run_uid: Uuid,
) -> Result<Option<ProcedureRunStatus>, HandlerError> {
    let Some(run) = procedure_runtime()
        .status(&scope, run_uid)
        .await
        .map_err(procedure_handler_error)?
    else {
        return Ok(None);
    };
    let node_runs = ArtifactRegistry::new(OrchestratorCtx::current_graph_pool())
        .list_node_runs(&scope, run_uid)
        .await
        .map_err(|error| procedure_handler_error(ProcedureError::Artifact(error)))?
        .into_iter()
        .map(|node_run| ProcedureNodeRunSummary {
            node_id: node_run.node_id,
            status: node_run.status.as_str().to_string(),
            started_at: node_run.started_at,
            completed_at: node_run.completed_at,
        })
        .collect();
    Ok(Some(ProcedureRunStatus {
        run_id: run.run_uid,
        session_id: run.session_id,
        current_node_id: run.current_node_id,
        status: run.status.as_str().to_string(),
        node_runs,
        output: run.output,
        error: run.error,
    }))
}

/// Returns a model-correctable rejection when `requested_ref` is not among the
/// procedure-capable skills selected for this turn, or `None` when the run may start.
///
/// `requested_ref` and the entries in `selected_procedure_skills` are both normalized
/// `skill://<name>` references, so the comparison is exact regardless of whether the
/// model named the skill bare or with the `skill://` prefix. The allowed list is
/// rendered from a [`BTreeSet`], which iterates in sorted order, so the message is
/// deterministic.
fn reject_unselected_procedure_skill(
    requested_ref: &str,
    selected_procedure_skills: &BTreeSet<String>,
) -> Option<ToolOutput> {
    if selected_procedure_skills.contains(requested_ref) {
        return None;
    }
    let allowed = selected_procedure_skills
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let message = if allowed.is_empty() {
        format!(
            "Procedure skill `{requested_ref}` cannot be started: no procedure-capable skill is selected for this turn. run_procedure only starts a procedure for a selected skill marked [procedure]."
        )
    } else {
        format!(
            "Procedure skill `{requested_ref}` is not among the selected procedure-capable skills for this turn. Allowed: {allowed}. Call run_procedure with one of those skills."
        )
    };
    Some(ToolOutput::error(message, Duration::ZERO))
}

fn procedure_runtime() -> ProcedureRuntime {
    ProcedureRuntime::new(ArtifactRegistry::new(OrchestratorCtx::current_graph_pool()))
}

/// Derives the session-participant identity that authorizes an agent-started run.
///
/// Mirrors the session-owner derivation used for other agent-initiated background
/// work (narration): it prefers the bound contact, then the recorded creating
/// identity, and returns `None` for anonymous sessions so a run is never started
/// without an authorizing principal.
fn session_identity(meta: &SessionMeta) -> Option<Identity> {
    if let Some(contact) = meta.contact.as_ref() {
        return Some(Identity {
            identity_type: IdentityType::Contact,
            id: contact.contact_id.0,
            tenant_id: meta.tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        });
    }
    match meta.created_by.as_ref()? {
        SessionActorRef::Identity { id } => Some(Identity {
            identity_type: IdentityType::Operator,
            id: *id,
            tenant_id: meta.tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        }),
        SessionActorRef::Contact { id } => Some(Identity {
            identity_type: IdentityType::Contact,
            id: id.0,
            tenant_id: meta.tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        }),
        SessionActorRef::Anonymous => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use moa_core::traits::IdentityType;
    use moa_core::{
        ContactId, RunProcedureToolInput, SessionActorRef, SessionMeta, TenantId,
        normalize_procedure_skill_ref,
    };
    use uuid::Uuid;

    use super::{ProcedureStartOutcome, reject_unselected_procedure_skill, session_identity};

    fn run_input(skill: &str) -> RunProcedureToolInput {
        RunProcedureToolInput {
            skill: skill.to_string(),
            input: serde_json::json!({}),
            idempotency_key: None,
        }
    }

    #[test]
    fn run_procedure_rejects_skill_outside_the_selected_set() {
        // Pins: a run_procedure call for a skill the turn did not select as
        // procedure-capable is rejected with the sorted allowed list and starts no run.
        let selected = BTreeSet::from([
            normalize_procedure_skill_ref("damaged-food-order"),
            normalize_procedure_skill_ref("transaction-dispute"),
        ]);

        let rejection = reject_unselected_procedure_skill(
            &run_input("refund-anything").procedure_ref(),
            &selected,
        )
        .expect("call to a non-selected skill is rejected");

        assert!(rejection.is_error);
        let text = rejection.to_text();
        assert!(
            text.contains("skill://refund-anything"),
            "names the rejected skill: {text}"
        );
        // Allowed skills are listed in sorted order for a deterministic message.
        assert!(
            text.contains("skill://damaged-food-order, skill://transaction-dispute"),
            "lists the allowed skills sorted: {text}"
        );
    }

    #[test]
    fn run_procedure_rejects_when_no_procedure_skill_is_selected() {
        // Pins: with no procedure-capable skill selected, any run_procedure call is
        // rejected and the message says none is available rather than listing an
        // empty allowlist.
        let selected = BTreeSet::new();

        let rejection =
            reject_unselected_procedure_skill(&run_input("anything").procedure_ref(), &selected)
                .expect("rejected when nothing is selected");

        assert!(rejection.is_error);
        let text = rejection.to_text();
        assert!(
            text.contains("no procedure-capable skill is selected"),
            "explains that nothing is selected: {text}"
        );
    }

    #[test]
    fn run_procedure_allows_selected_skill_named_bare_or_qualified() {
        // Pins: a call proceeds when its target is selected, whether the model named
        // the skill bare or as a skill:// reference, because both sides normalize to
        // the same canonical form.
        let selected = BTreeSet::from([normalize_procedure_skill_ref("damaged-food-order")]);

        assert!(
            reject_unselected_procedure_skill(
                &run_input("damaged-food-order").procedure_ref(),
                &selected
            )
            .is_none(),
            "bare selected skill name is allowed"
        );
        assert!(
            reject_unselected_procedure_skill(
                &run_input("skill://damaged-food-order").procedure_ref(),
                &selected
            )
            .is_none(),
            "already-qualified selected skill reference is allowed"
        );
    }

    #[test]
    fn session_identity_prefers_creator_then_none_for_anonymous() {
        // Pins: an agent-started run is authorized as the session's creating
        // identity, and an anonymous session yields no authorizing principal.
        let creator = Uuid::from_u128(7);
        let with_creator = SessionMeta {
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            created_by: Some(SessionActorRef::Identity { id: creator }),
            ..SessionMeta::default()
        };
        let identity = session_identity(&with_creator).expect("creator identity");
        assert_eq!(identity.identity_type, IdentityType::Operator);
        assert_eq!(identity.id, creator);

        let anonymous = SessionMeta {
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            created_by: Some(SessionActorRef::Anonymous),
            ..SessionMeta::default()
        };
        assert!(session_identity(&anonymous).is_none());
    }

    #[test]
    fn session_identity_prefers_bound_contact() {
        // Pins: a session bound to a contact authorizes the run as that contact.
        let contact_id = ContactId::new();
        let mut meta = SessionMeta {
            tenant_id: TenantId::from(Uuid::from_u128(2)),
            created_by: Some(SessionActorRef::Identity {
                id: Uuid::from_u128(9),
            }),
            ..SessionMeta::default()
        };
        meta.contact = Some(moa_test_support::fixtures::contact_ref_fixture(
            contact_id,
            meta.tenant_id,
            moa_core::ContactVerificationState::Verified,
        ));

        let identity = session_identity(&meta).expect("contact identity");
        assert_eq!(identity.identity_type, IdentityType::Contact);
        assert_eq!(identity.id, contact_id.0);
    }

    #[test]
    fn start_outcome_round_trips_across_the_durable_step_boundary() {
        // Pins: the start outcome journaled inside ctx.run survives serialization so
        // replay reproduces the same start/missing-inputs/rejected decision.
        for outcome in [
            ProcedureStartOutcome::Started {
                run_id: Uuid::from_u128(3),
                status: "queued".to_string(),
            },
            ProcedureStartOutcome::MissingInputs {
                missing: vec!["order_id".to_string()],
                invalid: vec!["quantity".to_string()],
            },
            ProcedureStartOutcome::Rejected {
                message: "skill artifact `skill://x` does not define a procedure".to_string(),
            },
        ] {
            let json = serde_json::to_string(&outcome).expect("serialize");
            let parsed: ProcedureStartOutcome = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed, outcome);
        }
    }
}
