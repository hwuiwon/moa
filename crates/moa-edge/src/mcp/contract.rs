//! Model-facing descriptions and output schemas for tenant-operation tools.

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::JsonObject;
use serde_json::{Map, Value, json};

use super::Server;

#[derive(Clone, Copy)]
enum DataKind {
    Object,
    Array,
    Null,
}

#[derive(Clone, Copy)]
struct ToolContract {
    returns: &'static str,
    next: &'static str,
    data_kind: DataKind,
}

impl ToolContract {
    const fn object(returns: &'static str, next: &'static str) -> Self {
        Self {
            returns,
            next,
            data_kind: DataKind::Object,
        }
    }

    const fn array(returns: &'static str, next: &'static str) -> Self {
        Self {
            returns,
            next,
            data_kind: DataKind::Array,
        }
    }

    const fn null(returns: &'static str, next: &'static str) -> Self {
        Self {
            returns,
            next,
            data_kind: DataKind::Null,
        }
    }
}

/// Replace terse macro-generated metadata with the complete model-facing contract.
pub(super) fn enrich(router: &mut ToolRouter<Server>) {
    for route in router.map.values_mut() {
        let Some(contract) = contract_for(route.attr.name.as_ref()) else {
            continue;
        };
        let use_when = route
            .attr
            .description
            .as_deref()
            .unwrap_or("Perform this tenant operation.")
            .trim_end_matches('.');
        let behavior = behavior_description(route.attr.annotations.as_ref());
        route.attr.description = Some(Cow::Owned(format!(
            "Use when: {use_when}. {behavior} Returns: {} Next: {} Tenant scope is always the authenticated tenant; never supply a tenant ID.",
            contract.returns, contract.next
        )));
        route.attr.output_schema = Some(Arc::new(output_schema(contract)));
    }
}

fn behavior_description(annotations: Option<&rmcp::model::ToolAnnotations>) -> &'static str {
    match annotations {
        Some(annotations) if annotations.read_only_hint == Some(true) => {
            "Side effects: None; this tool is read-only."
        }
        Some(annotations)
            if annotations.destructive_hint == Some(true)
                && annotations.open_world_hint == Some(true) =>
        {
            "Side effects: Changes tenant state and may execute model- or provider-backed work; inspect the target and evidence first."
        }
        Some(annotations) if annotations.destructive_hint == Some(true) => {
            "Side effects: Changes tenant state and is marked destructive; inspect the target first."
        }
        Some(annotations) if annotations.open_world_hint == Some(true) => {
            "Side effects: Creates or updates tenant state and may execute model- or provider-backed work."
        }
        Some(_) => "Side effects: Creates or updates tenant state without deleting the target.",
        None => "Side effects: Consult the tool annotations before calling.",
    }
}

fn output_schema(contract: ToolContract) -> JsonObject {
    let data = match contract.data_kind {
        DataKind::Object => json!({
            "type": "object",
            "description": contract.returns,
        }),
        DataKind::Array => json!({
            "type": "array",
            "description": contract.returns,
            "items": { "type": "object" },
        }),
        DataKind::Null => json!({
            "type": "null",
            "description": contract.returns,
        }),
    };
    Map::from_iter([
        ("type".to_string(), json!("object")),
        (
            "description".to_string(),
            json!(
                "Successful tool result. Execution failures instead set isError and return an error object."
            ),
        ),
        (
            "properties".to_string(),
            json!({
                "summary": {
                    "type": "string",
                    "description": "Concise human-readable outcome of this tool call."
                },
                "data": data
            }),
        ),
        ("required".to_string(), json!(["summary", "data"])),
        ("additionalProperties".to_string(), Value::Bool(false)),
    ])
}

#[rustfmt::skip]
fn contract_for(name: &str) -> Option<ToolContract> {
    let contract = match name {
        "analytics_catalog" => ToolContract::object("An analytics catalog object containing datasets and each dataset's allowed dimensions, measures, aggregations, and filter operators.", "Choose a catalog dataset and call `analytics_query`."),
        "analytics_query" => ToolContract::object("A bounded query result object containing typed columns, result rows, and execution metadata.", "Drill into relevant sessions with `sessions_list` or establish a baseline with an eval or experiment."),
        "sessions_list" => ToolContract::object("A page object with `sessions` dashboard-safe summaries and an optional opaque `next_cursor`.", "Call `session_get` for one returned session; pass `next_cursor` back here for another page."),
        "session_get" => ToolContract::object("One dashboard-safe session detail object with identity, lifecycle, timing, model, token, cost, and aggregate usage fields.", "Call `session_events_list` for the redacted timeline or `lineage_explain` for provenance."),
        "session_events_list" => ToolContract::object("A page object with redacted `events` and an optional opaque `next_cursor`; raw payloads and secrets are excluded.", "Use `lineage_explain` for a relevant session or turn ID, or fetch the next page with this tool."),
        "lineage_explain" => ToolContract::object("A lineage explanation object containing the matched subject and its tenant-scoped provenance records.", "Use the evidence to select an eval, experiment, or artifact change; do not infer missing raw content."),
        "learning_candidates_list" => ToolContract::array("An array of fresh redacted candidate summaries with candidate IDs, kinds, statuses, confidence, evidence counts, and timestamps.", "Call `learning_candidate_get` before accepting or rejecting a candidate."),

        "artifacts_list" => ToolContract::object("An artifact list object containing visible artifact summaries and their kinds, names, revisions, and lifecycle status.", "Call `artifact_export` to inspect an exact artifact before editing or publishing."),
        "artifact_export" => ToolContract::object("An exact artifact revision object containing source text, source format, metadata, and package files.", "Edit locally, then call `artifact_validate`; use `artifact_import` only after validation succeeds."),
        "artifact_validate" => ToolContract::object("A validation result with `valid` plus a report of schema and semantic errors; no artifact is written.", "If valid, call `artifact_import`; if invalid, correct the source and validate again."),
        "artifact_import" => ToolContract::object("A newly created draft artifact revision with its stable identity and exact revision UID; it is not active.", "Inspect or validate the draft, then call `artifact_publish` only when the exact revision is ready."),
        "artifact_publish" => ToolContract::object("The published artifact identity, revision UID, and resulting lifecycle state.", "Run the relevant eval or experiment and compare it with the baseline."),
        "learning_candidate_get" => ToolContract::object("The full tenant-scoped learning candidate with proposal, evidence, confidence, status, and review metadata.", "Call exactly one of `learning_candidate_accept_skill` or `learning_candidate_reject` after review."),
        "learning_candidate_accept_skill" => ToolContract::object("The recorded review result and resulting candidate or artifact state after regression and publish gates run.", "Verify the resulting skill with `artifacts_list`, then run the relevant eval or experiment."),
        "learning_candidate_reject" => ToolContract::object("The recorded rejection result and resulting candidate status; draft evidence remains preserved.", "Return to `learning_candidates_list` for remaining review work."),

        "agent_definitions_list" => ToolContract::object("A list response containing visible published agent definitions and their exact revision UIDs and statuses.", "Use `agent_revision_compare` before choosing a revision, or `agent_definition_install` to install it."),
        "agent_installations_list" => ToolContract::object("A list response containing this tenant's agent installations and currently deployed revisions.", "Call `agent_deployments_list` for history or `agent_definition_deploy` for an exact installation."),
        "agent_definition_install" => ToolContract::object("The created installation and initial deployment identifiers, selected revision UID, and resulting state.", "Confirm with `agent_installations_list`, then evaluate the installed behavior."),
        "agent_deployments_list" => ToolContract::object("A bounded deployment-history response for one installation, ordered by the owning service.", "Compare candidate revisions with `agent_revision_compare` before changing the deployment."),
        "agent_definition_deploy" => ToolContract::object("The new deployment record, installation UID, exact revision UID, and resulting active state.", "Confirm with `agent_deployments_list`, then run the relevant eval or simulation."),
        "agent_revision_compare" => ToolContract::object("A resolved structural diff between the base and new published revisions, including changed agent configuration and artifact references.", "Call `agent_revision_simulate` when the candidate warrants behavioral testing."),
        "agent_revision_simulate" => ToolContract::object("An accepted simulation run with its run UID, exact base/candidate revisions, and initial status.", "Poll with `experiment_status`, then call `agent_revision_simulation_compare` after completion."),
        "agent_revision_simulation_compare" => ToolContract::object("Per-variant execution and score results plus deltas from the named base variant for one simulation run.", "Deploy the supported revision with `agent_definition_deploy` or revise and simulate again."),
        "agent_principal_register" => ToolContract::object("The registered agent principal summary including agent ID, display name, tenant, status, and timestamps.", "Use `agent_principal_grant_act_as` only if the principal must act for a specific user."),
        "agent_principals_list" => ToolContract::array("An array of active agent principal summaries the caller is authorized to operate.", "Call `agent_principal_get` before changing one principal."),
        "agent_principal_get" => ToolContract::object("One authorized agent principal summary including identity, display name, tenant, status, and timestamps.", "Choose `agent_principal_grant_act_as`, `agent_principal_revoke_act_as`, or `agent_principal_deactivate` only if required."),
        "agent_principal_deactivate" => ToolContract::null("Null data confirming the principal was deactivated and its local credentials and delegation tuples were revoked.", "Call `agent_principals_list` to confirm it is no longer active."),
        "agent_principal_grant_act_as" => ToolContract::null("Null data confirming the exact agent-to-user delegation tuple was granted.", "Run the intended delegated workflow, or call `agent_principal_revoke_act_as` when no longer needed."),
        "agent_principal_revoke_act_as" => ToolContract::null("Null data confirming the exact agent-to-user delegation tuple was revoked.", "Call `agent_principal_get` or continue with other principal administration."),

        "capabilities_list" => ToolContract::object("A compiler-ready catalog of currently invocable versioned capabilities plus structured omission diagnostics.", "Select exact capability references and versions when compiling an execution run."),
        "execution_runs_list" => ToolContract::object("A bounded page of execution-run summaries plus the opaque keyset cursor for the next page.", "Call `execution_run_status` for one run or pass the returned cursor back here."),
        "execution_run_status" => ToolContract::object("The execution-run status with goal coverage, budget, aggregate task progress, completion checks, and terminal gaps.", "Poll again while active, or use `execution_review_decide`, `execution_signal`, or `execution_run_cancel` when the status requires it."),
        "execution_run_start" => ToolContract::object("The Session-owned pinned-template admission response containing the exact originating sequence and execution-run UID.", "Poll with `execution_run_status` using the returned run UID."),
        "execution_run_cancel" => ToolContract::object("The typed cancellation mutation result for the exact parent-scoped execution run.", "Confirm the terminal state with `execution_run_status`."),
        "execution_review_decide" => ToolContract::object("The typed review mutation result for the exact task generation.", "Poll with `execution_run_status` to observe the next task or terminal result."),
        "execution_signal" => ToolContract::object("The typed signal-delivery result for the exact waiting task generation.", "Poll with `execution_run_status` to observe progress."),

        "eval_suites_summarize" => ToolContract::object("A summary response for each inline TOML suite, including parsed identity, scenario counts, configuration references, and validation errors.", "Correct invalid suites, then call `eval_plan` before starting a run."),
        "eval_plan" => ToolContract::object("A dry-run plan containing resolved configs, scenario and run counts, evaluator selection, and estimated cost without executing models.", "If the plan is acceptable, call `eval_run` with explicit budgets."),
        "eval_datasets_list" => ToolContract::object("A list of tenant eval datasets with IDs, names, source URIs, row counts, hashes, and timestamps.", "Register missing data with `eval_dataset_register` or reference a returned dataset from a suite."),
        "eval_dataset_register" => ToolContract::object("The registered dataset identity, content hash, row count, source URI, and creation timestamp.", "Reference the dataset in an eval suite, then call `eval_plan`."),
        "eval_run" => ToolContract::object("The accepted hosted eval run ID, initial status, resolved suite identity, and effective execution budgets.", "Poll with `eval_run_status` using the returned run ID."),
        "eval_run_status" => ToolContract::object("The run lifecycle status, progress counts, timing, usage, cost, and failure details when present.", "Poll while active; after completion call `eval_scores`, then `eval_compare` against a baseline."),
        "eval_scores" => ToolContract::object("Score summaries for the run, grouped by configured evaluators and dimensions with counts and aggregate values.", "Call `eval_compare` with a baseline and candidate run."),
        "eval_compare" => ToolContract::object("Baseline-versus-candidate score rows with absolute values and deltas for two completed eval runs.", "Use the evidence to keep, revise, or reject the candidate artifact or agent revision."),

        "experiment_plan_generate" => ToolContract::object("A generated draft experiment-plan artifact identity, source, revision UID, and generation metadata; it is not published.", "Call `artifact_validate`, then `artifact_import` or `artifact_publish` as appropriate before `experiment_run`."),
        "experiments_list" => ToolContract::object("A bounded list response containing experiment run summaries, statuses, targets, variants, and timestamps.", "Call `experiment_status` for one run."),
        "experiment_run" => ToolContract::object("The admitted experiment run UID, initial status, resolved plan or inline configuration, and idempotency outcome.", "Poll with `experiment_status` using the returned run UID."),
        "experiment_status" => ToolContract::object("The experiment lifecycle status, trial counts, timing, target and variant metadata, and failure details when present.", "Poll while active; after completion call `experiment_scores` and `experiment_compare`."),
        "experiment_trials_list" => ToolContract::object("A bounded response containing trial summaries for one run, optionally filtered by status.", "Call `experiment_trial_status` for a trial needing detailed diagnosis."),
        "experiment_trial_status" => ToolContract::object("One trial's status, scenario and variant identity, timing, attempt, output, and failure details when present.", "Inspect sibling trials with `experiment_trials_list` or aggregate results with `experiment_scores`."),
        "experiment_cancel" => ToolContract::object("The cancellation acknowledgement, reason, and resulting experiment run status.", "Confirm the terminal state with `experiment_status`."),
        "experiment_scores" => ToolContract::object("Run-level, trial-level, and scenario-level score summaries grouped by metric and variant.", "Call `experiment_compare` with a baseline run."),
        "experiment_compare" => ToolContract::object("Baseline-versus-candidate run, scenario, and variant score rows with explicit deltas.", "If evidence supports a change, call `experiment_propose_improvements`; otherwise revise and rerun."),
        "experiment_propose_improvements" => ToolContract::object("Reviewable learning candidate IDs and proposal metadata derived from the completed experiment evidence.", "Call `learning_candidates_list`, then `learning_candidate_get` before any acceptance decision."),
        _ => return None,
    };
    Some(contract)
}
