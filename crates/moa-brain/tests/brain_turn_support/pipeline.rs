// No-memory context-pipeline support for brain turn tests.

#[allow(dead_code)]
fn build_no_memory_test_pipeline(
    config: &moa_core::MoaConfig,
    session_store: std::sync::Arc<dyn moa_core::SessionStore>,
) -> moa_brain::pipeline::ContextPipeline {
    build_no_memory_test_pipeline_with_tools(config, session_store, Vec::new())
}

fn build_no_memory_test_pipeline_with_tools(
    config: &moa_core::MoaConfig,
    session_store: std::sync::Arc<dyn moa_core::SessionStore>,
    tool_schemas: Vec<serde_json::Value>,
) -> moa_brain::pipeline::ContextPipeline {
    let history: Box<dyn moa_core::ContextProcessor> = Box::new(
        moa_brain::pipeline::history::HistoryCompiler::new(session_store.clone())
            .with_compaction_config(config.compaction.clone())
            .with_tool_output_config(config.tool_output.clone())
            .with_snapshot_config(config.context_snapshot.clone()),
    );
    let mut stages: Vec<Box<dyn moa_core::ContextProcessor>> = vec![
        Box::new(moa_brain::pipeline::identity::IdentityProcessor::default()),
        Box::new(moa_brain::pipeline::agent_instructions::AgentInstructionProcessor::new()),
        Box::new(moa_brain::pipeline::instructions::InstructionProcessor::new(
            config.general.workspace_instructions.clone(),
            config.general.user_instructions.clone(),
            None,
        )),
        Box::new(moa_brain::pipeline::tools::ToolDefinitionProcessor::new(
            tool_schemas,
        )),
    ];
    stages.extend([
        history,
        Box::new(moa_brain::pipeline::delegation_planning::DelegationPlanningProcessor::new())
            as Box<dyn moa_core::ContextProcessor>,
        Box::new(moa_brain::pipeline::runtime_context::RuntimeContextProcessor::default())
            as Box<dyn moa_core::ContextProcessor>,
    ]);

    moa_brain::pipeline::ContextPipeline::with_runtime_limits(
        stages,
        config.budgets.daily_tenant_cents,
        config.context_snapshot.clone(),
    )
}
