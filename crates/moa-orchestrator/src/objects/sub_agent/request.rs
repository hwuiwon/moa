//! Completion-request construction helpers for sub-agent turns.

use super::*;

pub(super) fn build_completion_request(
    state: &SubAgentVoState,
) -> Result<CompletionRequest, HandlerError> {
    let model = state
        .model
        .clone()
        .ok_or_else(|| TerminalError::new("sub-agent model missing"))?;
    let capabilities = configured_model_capabilities(&model)?;
    let max_output_tokens = clamp_sub_agent_max_output(&capabilities, state.budget_remaining);
    let mut request = CompletionRequest {
        model: Some(model),
        messages: vec![
            ContextMessage::system(SUB_AGENT_SYSTEM_PROMPT),
            ContextMessage::user(sub_agent_context_prompt(state)),
        ],
        tools: filtered_tool_schemas(&state.tool_subset)?,
        max_output_tokens: Some(max_output_tokens),
        temperature: None,
        response_format: None,
        metadata: HashMap::new(),
    };
    request
        .metadata
        .insert("_moa.sub_agent_id".to_string(), json!(state.task_hash()));
    Ok(request)
}

fn clamp_sub_agent_max_output(capabilities: &ModelCapabilities, budget_remaining: u64) -> usize {
    let budget_remaining = usize::try_from(budget_remaining).unwrap_or(usize::MAX);
    capabilities.max_output.min(budget_remaining)
}

pub(super) fn filtered_tool_schemas(
    tool_subset: &[String],
) -> Result<Vec<serde_json::Value>, HandlerError> {
    let configured = OrchestratorCtx::current_tool_schemas();
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
    for schema in delegation_tool_schemas() {
        if let Some(name) = schema.get("name").and_then(serde_json::Value::as_str)
            && allowed.contains(name)
        {
            tools.push(schema);
        }
    }
    Ok(tools)
}

pub(super) fn configured_model_capabilities(
    model: &ModelId,
) -> Result<ModelCapabilities, HandlerError> {
    OrchestratorCtx::current_provider_registry()
        .capabilities_for_model(Some(model.as_str()))
        .map_err(to_handler_error)
}

pub(super) fn synthetic_session_meta(state: &SubAgentVoState) -> Result<SessionMeta, HandlerError> {
    let tenant_id = state
        .tenant_id
        .ok_or_else(|| TerminalError::new("sub-agent tenant_id missing"))?;
    Ok(SessionMeta {
        id: state
            .parent_session
            .ok_or_else(|| TerminalError::new("sub-agent parent session missing"))?,
        tenant_id,
        model: state
            .model
            .clone()
            .ok_or_else(|| TerminalError::new("sub-agent model missing"))?,
        status: SessionStatus::Running,
        updated_at: Utc::now(),
        ..SessionMeta::default()
    })
}

const SUB_AGENT_SYSTEM_PROMPT: &str = "\
You are a specialist sub-agent working for a parent agent session.
Complete only the delegated task and do not broaden scope without a parent follow-up.
Use tools only when they materially advance the delegated task.
Do not perform destructive or write-heavy work unless the task explicitly authorizes it.

Final result to parent:
- State the outcome and the evidence that supports it.
- Include relevant file paths, command results, or unresolved questions.
- Summarize; do not return raw logs unless the parent specifically requested them.";

fn sub_agent_context_prompt(state: &SubAgentVoState) -> String {
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
    fn sub_agent_request_keeps_task_context_out_of_stable_system_prompt() {
        // Pins: delegated sub-agent prompts preserve scope, allowed tools, and evidence-bearing final output.
        let state = SubAgentVoState {
            task: Some("Inspect auth.rs for token refresh races.".to_string()),
            tool_subset: vec!["grep".to_string(), "file_read".to_string()],
            ..SubAgentVoState::default()
        };

        assert!(SUB_AGENT_SYSTEM_PROMPT.contains("Complete only the delegated task"));
        assert!(SUB_AGENT_SYSTEM_PROMPT.contains("State the outcome and the evidence"));
        assert!(SUB_AGENT_SYSTEM_PROMPT.contains("do not return raw logs"));
        assert!(!SUB_AGENT_SYSTEM_PROMPT.contains("Inspect auth.rs"));
        assert!(!SUB_AGENT_SYSTEM_PROMPT.contains("Allowed tools: grep"));

        let prompt = sub_agent_context_prompt(&state);
        assert!(prompt.contains("Inspect auth.rs for token refresh races."));
        assert!(prompt.contains("Allowed tools: grep, file_read"));
    }

    #[test]
    fn max_output_tokens_are_clamped_to_remaining_child_budget() {
        // Pins: a child cannot ask the provider for more output tokens than its remaining budget.
        let capabilities = ModelCapabilities {
            max_output: 4096,
            ..ModelCapabilities::default()
        };

        assert_eq!(clamp_sub_agent_max_output(&capabilities, 512), 512);
        assert_eq!(clamp_sub_agent_max_output(&capabilities, 8192), 4096);
    }
}
