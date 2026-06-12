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
        messages: vec![ContextMessage::system(sub_agent_system_prompt(state))],
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
    let configured = OrchestratorCtx::current().tool_schemas.clone();
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
    if allowed.contains("dispatch_sub_agent") {
        tools.push(dispatch_sub_agent_tool_schema());
    }
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
    OrchestratorCtx::current()
        .providers
        .capabilities_for_model(Some(model.as_str()))
        .map_err(to_handler_error)
}

pub(super) fn synthetic_session_meta(state: &SubAgentVoState) -> Result<SessionMeta, HandlerError> {
    Ok(SessionMeta {
        id: state
            .parent_session
            .ok_or_else(|| TerminalError::new("sub-agent parent session missing"))?,
        workspace_id: state
            .workspace_id
            .clone()
            .ok_or_else(|| TerminalError::new("sub-agent workspace_id missing"))?,
        user_id: state
            .user_id
            .clone()
            .ok_or_else(|| TerminalError::new("sub-agent user_id missing"))?,
        model: state
            .model
            .clone()
            .ok_or_else(|| TerminalError::new("sub-agent model missing"))?,
        status: SessionStatus::Running,
        updated_at: Utc::now(),
        ..SessionMeta::default()
    })
}

fn sub_agent_system_prompt(state: &SubAgentVoState) -> String {
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
        "You are a specialist sub-agent working for a parent MOA session.\n\
         Complete only the delegated task and do not broaden scope without a parent follow-up.\n\n\
         Task:\n{task}\n\n\
         Tool policy:\n\
         - {tools}\n\
         - Use tools only when they materially advance the delegated task.\n\
         - Do not perform destructive or write-heavy work unless the task explicitly authorizes it.\n\n\
         Final result to parent:\n\
         - State the outcome and the evidence that supports it.\n\
         - Include relevant file paths, command results, or unresolved questions.\n\
         - Summarize; do not return raw logs unless the parent specifically requested them."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_agent_system_prompt_pins_scope_tools_and_evidence_contract() {
        // Pins: delegated sub-agent prompts preserve scope, allowed tools, and evidence-bearing final output.
        let state = SubAgentVoState {
            task: Some("Inspect auth.rs for token refresh races.".to_string()),
            tool_subset: vec!["grep".to_string(), "file_read".to_string()],
            ..SubAgentVoState::default()
        };

        let prompt = sub_agent_system_prompt(&state);

        assert!(prompt.contains("Complete only the delegated task"));
        assert!(prompt.contains("Inspect auth.rs for token refresh races."));
        assert!(prompt.contains("Allowed tools: grep, file_read"));
        assert!(prompt.contains("State the outcome and the evidence that supports it."));
        assert!(prompt.contains("unresolved questions"));
        assert!(prompt.contains("do not return raw logs"));
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
