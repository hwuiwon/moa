//! Review-time skill-regression gate orchestration.

use std::sync::Arc;

use moa_artifacts::registry::ArtifactRegistry;
use moa_config::MoaConfig;
use moa_core::{
    error::{MoaError, Result},
    types::{action_policy::ActionRuleScope, experience::LearningCandidate, provider::ModelTask},
};
use moa_eval_core::TestSuite;
use moa_execution::repository::{CompileAuditWriteOutcome, ExecutionRepository, ExecutionScope};
use moa_providers::ProviderRegistry;
use moa_session::PostgresSessionStore;
use moa_skills::{
    artifact::skill_definition_from_package,
    package::{SkillPackage, SkillPackageFile},
    regression::compare_scores,
};
use serde_json::json;

use crate::services::execution::capability_catalog::resolve_skill_regression_compile_authority;

use super::{
    DEFAULT_SKILL_SUITE_TIMEOUT_SECONDS, DEFAULT_SKILL_TEST_BUDGET_DOLLARS,
    SkillRegressionCompileContext, SkillRegressionExecution, SkillRegressionGate,
    compilation::{
        SkillTemplateCompileRequest, compile_skill_execution_template, draft_artifact_revision_uid,
        validated_suite_hash,
    },
    report::{previous_skill_payload, regression_summary_to_json, run_failures_json},
    runner::{
        estimate_suite_cost, execute_candidate_only, execute_previous_and_candidate,
        run_has_execution_failure, summarize_regression_run,
    },
    suite::{
        GeneratedSuiteReport, collect_held_out_pool, generated_suite_contribution,
        resolve_regression_execution_input, skill_name,
    },
};

/// Executes the complete review-time regression gate for one proposed skill revision.
pub(super) async fn evaluate(
    config: MoaConfig,
    providers: Arc<ProviderRegistry>,
    store: Arc<PostgresSessionStore>,
    scope: ActionRuleScope,
    candidate: LearningCandidate,
    compile_context: SkillRegressionCompileContext,
) -> Result<SkillRegressionGate> {
    // Review input is assembled from the artifact owner's contribution rows, not
    // from candidate payload JSON. The bytes are attributable storage: one
    // `generated` row for the candidate's own suite and one `accumulated` row per
    // deduped sibling session, each naming the session and experience it came
    // from so an erasure can reach them.
    let contributions = ArtifactRegistry::new(store.pool().clone())
        .list_suite_contributions(&scope, candidate.id)
        .await?;
    let Some(generated_suite) = generated_suite_contribution(&contributions) else {
        return Ok(SkillRegressionGate::blocked(
            json!({
                "regression_execution": "unavailable",
                "runner": "moa-eval",
                "reason": "candidate has no generated regression suite",
                "generated_suite": null,
                "previous_skill": null,
            }),
            "candidate has no generated regression suite".to_string(),
        ));
    };

    let Some(skill_name) = skill_name(&candidate) else {
        return Ok(SkillRegressionGate::blocked(
            json!({
                "regression_execution": "unavailable",
                "runner": "moa-eval",
                "reason": "candidate payload missing skill name",
                "generated_suite": generated_suite.summary(),
                "previous_skill": null,
            }),
            "candidate payload missing skill name".to_string(),
        ));
    };

    let previous_package = compile_context.previous_package;
    let previous_skill = previous_package
        .as_ref()
        .map(|package| previous_skill_payload(&package.skill));

    let mut suite = match toml::from_str::<TestSuite>(&generated_suite.suite_source) {
        Ok(suite) => suite,
        Err(error) => {
            return Ok(SkillRegressionGate::blocked(
                json!({
                    "regression_execution": "unavailable",
                    "runner": "moa-eval",
                    "reason": "generated regression suite could not be parsed",
                    "error": error.to_string(),
                    "generated_suite": generated_suite.summary(),
                    "previous_skill": previous_skill,
                }),
                "generated regression suite could not be parsed".to_string(),
            ));
        }
    };
    if suite.cases.is_empty() {
        return Ok(SkillRegressionGate::blocked(
            json!({
                "regression_execution": "unavailable",
                "runner": "moa-eval",
                "reason": "generated regression suite contains no test cases",
                "generated_suite": generated_suite.summary_with_suite(&suite),
                "previous_skill": previous_skill,
            }),
            "generated regression suite contains no test cases".to_string(),
        ));
    }
    if suite.name.trim().is_empty() {
        return Ok(SkillRegressionGate::blocked(
            json!({
                "regression_execution": "unavailable",
                "runner": "moa-eval",
                "reason": "generated regression suite has no name",
                "generated_suite": generated_suite.summary_with_suite(&suite),
                "previous_skill": previous_skill,
            }),
            "generated regression suite has no name".to_string(),
        ));
    }
    // A missing suite timeout parses as zero, which times every case out
    // instantly and rejects the candidate for a fixture defect rather than a
    // behavior regression. Floor it instead of trusting the TOML default.
    if suite.default_timeout_seconds == 0 {
        suite.default_timeout_seconds = DEFAULT_SKILL_SUITE_TIMEOUT_SECONDS;
    }

    let candidate_package = SkillPackage::new(
        compile_context
            .draft_files
            .into_iter()
            .map(|file| SkillPackageFile {
                path: file.path,
                content: file.content,
                content_type: file.content_type,
                executable: file.executable,
            })
            .collect(),
    )
    .validate()?;
    let candidate_markdown = candidate_package.skill_md.clone();

    let definition = skill_definition_from_package(&candidate_package)?;
    let compile_operation_key = if let Some(template) = definition.execution_plan.as_ref() {
        let draft_revision_uid = draft_artifact_revision_uid(&candidate)?;
        let suite_hash = validated_suite_hash(&suite)?;
        let operation_key = format!("skill_regression:{draft_revision_uid}:{suite_hash}");
        let deployment_catalog = compile_context.router.activated_catalog();
        let authority = resolve_skill_regression_compile_authority(
            store.pool().clone(),
            deployment_catalog.capability_registrations(),
            scope,
            compile_context.draft,
        )
        .await?;
        let run_input = resolve_regression_execution_input(&suite)?;
        let compiled = compile_skill_execution_template(SkillTemplateCompileRequest {
            config: &config,
            tenant_id: candidate.tenant_id,
            skill_name: &skill_name,
            skill_input_schema: &definition.inputs,
            template,
            run_input: &run_input,
            catalog: &authority.catalog,
            authorization: &authority.authorization,
            operation_key: &operation_key,
        })?;
        let audit_outcome = ExecutionRepository::new(store.pool().clone())
            .write_compile_audit(
                ExecutionScope::Tenant {
                    tenant_id: candidate.tenant_id,
                },
                &compiled.audit,
            )
            .await
            .map_err(|error| {
                MoaError::StorageError(format!(
                    "skill regression compile audit persistence failed: {error}"
                ))
            })?;
        moa_brain::execution_planning::request::record_applied_planning_audit(&audit_outcome);
        if matches!(audit_outcome, CompileAuditWriteOutcome::Conflict { .. }) {
            return Err(MoaError::ValidationError(format!(
                "skill regression planning audit conflicts for operation `{operation_key}`"
            )));
        }
        if !compiled.accepted {
            return Ok(SkillRegressionGate::blocked(
                json!({
                    "regression_execution": "unavailable",
                    "runner": "moa-eval",
                    "reason": "candidate execution-plan template failed compilation",
                    "generated_suite": generated_suite.summary_with_suite(&suite),
                    "previous_skill": previous_skill,
                    "compile_operation_key": operation_key,
                }),
                "candidate execution-plan template failed compilation".to_string(),
            )
            .with_compile_operation_key(Some(operation_key)));
        }
        Some(operation_key)
    } else {
        None
    };

    // An unavailable provider is an operational failure, not a property of the
    // candidate: surface an error so the review can be retried after the
    // deployment is fixed, instead of silently waiving the gate.
    let provider =
        providers.provider_for_model(Some(config.model_for_task(ModelTask::MainLoop)))?;

    // Held-out material the candidate was not derived from: the previous
    // promoted revision's own suite (it rode that revision's package) plus
    // sibling suites accumulated from deduped recurring sessions.
    let held_out = collect_held_out_pool(previous_package.as_ref(), &contributions);

    let run_count = if previous_package.is_some() { 2.0 } else { 1.0 };
    let mut estimated_cost = estimate_suite_cost(&suite, provider.as_ref()) * run_count;
    if let Some(pool_suite) = &held_out.suite {
        estimated_cost += estimate_suite_cost(pool_suite, provider.as_ref()) * run_count;
    }
    if estimated_cost > DEFAULT_SKILL_TEST_BUDGET_DOLLARS {
        return Ok(SkillRegressionGate::blocked(
            json!({
                "regression_execution": "unavailable",
                "runner": "moa-eval",
                "reason": "estimated regression cost exceeds budget",
                "estimated_cost_dollars": estimated_cost,
                "budget_dollars": DEFAULT_SKILL_TEST_BUDGET_DOLLARS,
                "generated_suite": generated_suite.summary_with_suite(&suite),
                "previous_skill": previous_skill,
            }),
            "estimated regression cost exceeds the review budget".to_string(),
        )
        .with_compile_operation_key(compile_operation_key));
    }

    let Some(previous_package) = previous_package else {
        // First revision of a new skill: nothing to compare against, so the
        // candidate suite runs alone as a smoke gate instead of being skipped,
        // and any sibling suites run as true held-out material.
        let candidate_run = execute_candidate_only(
            config.clone(),
            suite.clone(),
            skill_name.clone(),
            candidate_markdown.clone(),
            provider.clone(),
        )
        .await?;
        let candidate_summary = summarize_regression_run(&candidate_run);
        let has_execution_failure = run_has_execution_failure(&candidate_run);
        let held_in_accepted = !has_execution_failure && candidate_summary.failed_runs == 0;

        let mut held_out_report = held_out.report_base();
        let mut held_out_accepted = true;
        if let Some(pool_suite) = &held_out.suite {
            let pool_run = execute_candidate_only(
                config,
                pool_suite.clone(),
                skill_name,
                candidate_markdown,
                provider,
            )
            .await?;
            let pool_summary = summarize_regression_run(&pool_run);
            // Sibling suites come from resolved sessions of the same task, so
            // the candidate is expected to pass them outright.
            held_out_accepted = pool_summary.failed_runs == 0;
            held_out_report["decision"] = json!(if held_out_accepted {
                "accepted"
            } else {
                "rejected"
            });
            held_out_report["candidate"] = regression_summary_to_json(&pool_summary);
            held_out_report["candidate_failures"] = run_failures_json(&pool_run);
        }

        let accepted = held_in_accepted && held_out_accepted;
        let report = json!({
            "regression_execution": "completed",
            "execution_mode": SkillRegressionExecution::CandidateOnly.as_str(),
            "runner": "moa-eval",
            "decision": if has_execution_failure {
                "eval_failed"
            } else if accepted {
                "accepted"
            } else {
                "rejected"
            },
            "generated_suite": generated_suite.summary_with_suite(&suite),
            "previous_skill": null,
            "candidate": regression_summary_to_json(&candidate_summary),
            "candidate_failures": run_failures_json(&candidate_run),
            "held_out": held_out_report,
        });
        return Ok(if accepted {
            SkillRegressionGate::accepted(
                report,
                SkillRegressionExecution::CandidateOnly,
                held_out.source_count,
            )
            .with_compile_operation_key(compile_operation_key)
        } else {
            SkillRegressionGate {
                report,
                allow_promotion: false,
                rejection_reason: Some(if has_execution_failure {
                    "skill regression eval failed".to_string()
                } else if !held_in_accepted {
                    "candidate skill failed its generated regression suite".to_string()
                } else {
                    "candidate skill failed the held-out sibling suites".to_string()
                }),
                execution: SkillRegressionExecution::Blocked,
                held_out_sources: held_out.source_count,
                compile_operation_key,
            }
        });
    };
    let previous_markdown = previous_package.skill_markdown()?.to_string();

    let executed = execute_previous_and_candidate(
        config.clone(),
        suite.clone(),
        skill_name.clone(),
        previous_markdown.clone(),
        candidate_markdown.clone(),
        provider.clone(),
    )
    .await?;

    let previous_summary = summarize_regression_run(&executed.previous);
    let candidate_summary = summarize_regression_run(&executed.candidate);
    let has_execution_failure = run_has_execution_failure(&executed.previous)
        || run_has_execution_failure(&executed.candidate);
    let held_in_accepted =
        !has_execution_failure && compare_scores(&previous_summary, &candidate_summary);

    let mut held_out_report = held_out.report_base();
    let mut held_out_accepted = true;
    if let Some(pool_suite) = &held_out.suite {
        let pool = execute_previous_and_candidate(
            config,
            pool_suite.clone(),
            skill_name,
            previous_markdown,
            candidate_markdown,
            provider,
        )
        .await?;
        let pool_previous = summarize_regression_run(&pool.previous);
        let pool_candidate = summarize_regression_run(&pool.candidate);
        // No separate execution-failure rejection here: a stale pooled case
        // that errors for environmental reasons fails both runs equally and
        // the comparison neutralizes it. Only the candidate doing worse than
        // the previous revision on material it never saw is a regression.
        held_out_accepted = compare_scores(&pool_previous, &pool_candidate);
        held_out_report["decision"] = json!(if held_out_accepted {
            "accepted"
        } else {
            "rejected"
        });
        held_out_report["previous"] = regression_summary_to_json(&pool_previous);
        held_out_report["previous_failures"] = run_failures_json(&pool.previous);
        held_out_report["candidate"] = regression_summary_to_json(&pool_candidate);
        held_out_report["candidate_failures"] = run_failures_json(&pool.candidate);
    }

    let accepted = held_in_accepted && held_out_accepted;
    let report = json!({
        "regression_execution": "completed",
        "execution_mode": SkillRegressionExecution::ComparedWithPrevious.as_str(),
        "runner": "moa-eval",
        "decision": if has_execution_failure {
            "eval_failed"
        } else if accepted {
            "accepted"
        } else {
            "rejected"
        },
        "generated_suite": generated_suite.summary_with_suite(&suite),
        "previous_skill": previous_skill_payload(&previous_package.skill),
        "previous": regression_summary_to_json(&previous_summary),
        "previous_failures": run_failures_json(&executed.previous),
        "candidate": regression_summary_to_json(&candidate_summary),
        "candidate_failures": run_failures_json(&executed.candidate),
        "held_out": held_out_report,
    });

    if accepted {
        Ok(SkillRegressionGate::accepted(
            report,
            SkillRegressionExecution::ComparedWithPrevious,
            held_out.source_count,
        )
        .with_compile_operation_key(compile_operation_key))
    } else {
        Ok(SkillRegressionGate {
            report,
            allow_promotion: false,
            rejection_reason: Some(if has_execution_failure {
                "skill regression eval failed".to_string()
            } else if !held_in_accepted {
                "skill regression rejected the proposed draft".to_string()
            } else {
                "candidate regressed on the held-out suite pool".to_string()
            }),
            execution: SkillRegressionExecution::Blocked,
            held_out_sources: held_out.source_count,
            compile_operation_key,
        })
    }
}
