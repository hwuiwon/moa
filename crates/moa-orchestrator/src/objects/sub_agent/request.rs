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
    let mut request = CompletionRequest {
        model: Some(model),
        messages: vec![ContextMessage::system(sub_agent_system_prompt(state))],
        tools: filtered_tool_schemas(&state.tool_subset)?,
        max_output_tokens: Some(capabilities.max_output),
        temperature: None,
        response_format: None,
        cache_breakpoints: Vec::new(),
        cache_controls: Vec::new(),
        metadata: HashMap::new(),
    };
    request
        .metadata
        .insert("_moa.sub_agent_id".to_string(), json!(state.task_hash()));
    Ok(request)
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
         Complete the delegated task precisely and return a concise final result to the parent.\n\
         Task: {task}\n\
         {tools}"
    )
}
