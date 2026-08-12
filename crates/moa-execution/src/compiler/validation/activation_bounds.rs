//! Activation and dispatch bounds for durable long-horizon execution plans.

use moa_artifacts::execution_plan::{
    CompletionCheckKind, ExecutionGoalContract, ExecutionPlanDefinition,
};
use moa_config::ExecutionConfig;

use crate::compiler::ExecutionValidationReport;

/// Rejects completion metadata that cannot be evaluated within bounded activations.
pub(in crate::compiler) fn validate_completion_activation_bounds(
    goal: &ExecutionGoalContract,
    config: &ExecutionConfig,
    report: &mut ExecutionValidationReport,
) {
    let metadata_count = goal
        .requirements
        .len()
        .saturating_add(goal.constraints.len())
        .saturating_add(goal.deliverables.len())
        .saturating_add(goal.coverage.len())
        .saturating_add(goal.completion_checks.len());
    if metadata_count > config.maximum_activation_steps {
        report.error(
            "completion_metadata_exceeds_activation_bound",
            "goal",
            format!(
                "completion metadata count {metadata_count} exceeds one activation bound {}",
                config.maximum_activation_steps
            ),
        );
    }
    let referenced_node_count = goal
        .completion_checks
        .iter()
        .map(|check| match &check.kind {
            CompletionCheckKind::RequiredNodes { node_ids }
            | CompletionCheckKind::Citations { node_ids, .. } => node_ids.len(),
            CompletionCheckKind::MapCoverage { .. } => 1,
            CompletionCheckKind::OutputSchema | CompletionCheckKind::AgentVerifier { .. } => 0,
        })
        .fold(0_usize, usize::saturating_add);
    if referenced_node_count > config.maximum_activation_steps {
        report.error(
            "completion_node_references_exceed_activation_bound",
            "goal.completion_checks",
            format!(
                "completion node-reference count {referenced_node_count} exceeds one activation bound {}",
                config.maximum_activation_steps
            ),
        );
    }
    let verifier_count = goal
        .completion_checks
        .iter()
        .filter(|check| matches!(check.kind, CompletionCheckKind::AgentVerifier { .. }))
        .count();
    if verifier_count > config.dispatch_batch_size {
        report.error(
            "completion_verifiers_exceed_dispatch_bound",
            "goal.completion_checks",
            format!(
                "completion verifier count {verifier_count} exceeds one dispatch batch {}",
                config.dispatch_batch_size
            ),
        );
    }
}

/// Rejects plans whose node-state seed or aggregate projection cannot fit one activation bound.
pub(in crate::compiler) fn validate_plan_activation_bound(
    plan: &ExecutionPlanDefinition,
    config: &ExecutionConfig,
    report: &mut ExecutionValidationReport,
) {
    if plan.nodes.len() > config.maximum_activation_steps {
        report.error(
            "plan_nodes_exceed_activation_bound",
            "plan.nodes",
            format!(
                "plan node count {} exceeds one activation bound {}; high-cardinality work must use paged map or reduce tasks",
                plan.nodes.len(),
                config.maximum_activation_steps
            ),
        );
    }
}
