//! Completion-request construction helpers for worker turns.

use super::*;

pub(super) fn build_completion_request(
    state: &WorkerVoState,
    providers: &ProviderRegistry,
    configured_tool_schemas: &[serde_json::Value],
) -> Result<CompletionRequest, HandlerError> {
    let model = state
        .model
        .clone()
        .ok_or_else(|| TerminalError::new("worker model missing"))?;
    let capabilities = configured_model_capabilities(providers, &model)?;
    let max_output_tokens = clamp_worker_max_output(&capabilities, state.budget_remaining);
    let mut request = CompletionRequest {
        model: Some(model),
        messages: vec![
            ContextMessage::system(WORKER_SYSTEM_PROMPT),
            ContextMessage::user(worker_context_prompt(state)),
        ],
        tools: filtered_tool_schemas(configured_tool_schemas, &state.tool_subset)?,
        max_output_tokens: Some(max_output_tokens),
        temperature: None,
        response_format: None,
        metadata: HashMap::new(),
    };
    request
        .metadata
        .insert("_moa.worker_id".to_string(), json!(state.task_hash()));
    Ok(request)
}

fn clamp_worker_max_output(capabilities: &ModelCapabilities, budget_remaining: u64) -> usize {
    let budget_remaining = usize::try_from(budget_remaining).unwrap_or(usize::MAX);
    capabilities.max_output.min(budget_remaining)
}

pub(super) fn filtered_tool_schemas(
    configured: &[serde_json::Value],
    tool_subset: &[String],
) -> Result<Vec<serde_json::Value>, HandlerError> {
    let allowed = tool_subset
        .iter()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let mut tools = configured
        .iter()
        .filter(|schema| {
            schema
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| allowed.contains(name))
        })
        .cloned()
        .collect::<Vec<_>>();
    // Child-only report tools (`report_to_parent`, `request_input`) are core upward
    // communication primitives, so every child gets them regardless of the task-specific
    // tool subset the parent granted. They are never merged onto the root session.
    tools.extend(child_report_tool_schemas());
    Ok(tools)
}

pub(super) fn configured_model_capabilities(
    providers: &ProviderRegistry,
    model: &ModelId,
) -> Result<ModelCapabilities, HandlerError> {
    providers
        .capabilities_for_model(Some(model.as_str()))
        .map_err(moa_error_to_handler_error)
}

pub(super) fn synthetic_session_meta(state: &WorkerVoState) -> Result<SessionMeta, HandlerError> {
    let tenant_id = state
        .tenant_id
        .ok_or_else(|| TerminalError::new("worker tenant_id missing"))?;
    Ok(SessionMeta {
        id: state
            .parent_session
            .ok_or_else(|| TerminalError::new("worker parent session missing"))?,
        tenant_id,
        model: state
            .model
            .clone()
            .ok_or_else(|| TerminalError::new("worker model missing"))?,
        status: SessionStatus::Running,
        updated_at: Utc::now(),
        ..SessionMeta::default()
    })
}

const WORKER_SYSTEM_PROMPT: &str = "\
You are a specialist worker working for a parent agent session.
Complete only the delegated task and do not broaden scope without a parent follow-up.
Use tools only when they materially advance the delegated task.
Do not perform destructive or write-heavy work unless the task explicitly authorizes it.

Final result to parent:
- State the outcome and the evidence that supports it.
- Include relevant file paths, command results, or unresolved questions.
- Summarize; do not return raw logs unless the parent specifically requested them.";

fn worker_context_prompt(state: &WorkerVoState) -> String {
    let task = state
        .task
        .as_deref()
        .unwrap_or("Complete the delegated task.");
    let tools = if state.tool_subset.is_empty() {
        "No tools are available.".to_string()
    } else {
        format!("Allowed tools: {}", state.tool_subset.join(", "))
    };
    format!(
        "Task:\n{task}\n\n\
         Tool policy:\n\
         - {tools}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_request_keeps_task_context_out_of_stable_system_prompt() {
        // Pins: delegated worker prompts preserve scope, allowed tools, and evidence-bearing final output.
        let state = WorkerVoState {
            task: Some("Inspect auth.rs for token refresh races.".to_string()),
            tool_subset: vec!["grep".to_string(), "file_read".to_string()],
            ..WorkerVoState::default()
        };

        assert!(WORKER_SYSTEM_PROMPT.contains("Complete only the delegated task"));
        assert!(WORKER_SYSTEM_PROMPT.contains("State the outcome and the evidence"));
        assert!(WORKER_SYSTEM_PROMPT.contains("do not return raw logs"));
        assert!(!WORKER_SYSTEM_PROMPT.contains("Inspect auth.rs"));
        assert!(!WORKER_SYSTEM_PROMPT.contains("Allowed tools: grep"));

        let prompt = worker_context_prompt(&state);
        assert!(prompt.contains("Inspect auth.rs for token refresh races."));
        assert!(prompt.contains("Allowed tools: grep, file_read"));
    }

    #[test]
    fn child_report_tools_are_appended_and_disjoint_from_root_delegation_set() {
        // Pins: the schemas appended to every child subset are exactly the two child-only
        // report tools, and none of them leak into the root session's delegation set.
        // The appended set and its disjointness from root are pinned at the schema layer.
        let child_report_names = moa_core::types::worker::tool_schema::child_report_tool_schemas()
            .iter()
            .filter_map(|schema| schema.get("name").and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            child_report_names,
            vec!["report_to_parent", "request_input"]
        );

        let root_names = moa_core::types::worker::tool_schema::delegation_tool_schemas()
            .iter()
            .filter_map(|schema| schema.get("name").and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .collect::<Vec<_>>();
        for child_report in &child_report_names {
            assert!(
                !root_names.contains(child_report),
                "root delegation set must not expose child-only tool {child_report}"
            );
        }
    }

    #[test]
    fn max_output_tokens_are_clamped_to_remaining_child_budget() {
        // Pins: a child cannot ask the provider for more output tokens than its remaining budget.
        let capabilities = ModelCapabilities {
            max_output: 4096,
            ..ModelCapabilities::default()
        };

        assert_eq!(clamp_worker_max_output(&capabilities, 512), 512);
        assert_eq!(clamp_worker_max_output(&capabilities, 8192), 4096);
    }
}
