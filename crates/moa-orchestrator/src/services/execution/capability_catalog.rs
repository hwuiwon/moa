//! Compiler capability-catalog projection and artifact authority loading.

use super::planning_context::{canonical_skill_policy_ref, skill_revision_ref};
use super::support::invalid_execution_request;
use super::*;

pub(super) async fn list_capabilities_inner(
    pool: sqlx::PgPool,
    registrations: Vec<(ToolDefinition, ToolExecution)>,
    request: CapabilitiesListRequest,
) -> Result<CapabilitiesListResponse, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let registry = ArtifactRegistry::new(pool.clone());
    let revisions = load_serving_revisions(&registry, &scope)
        .await
        .map_err(moa_error_to_status_handler_error)?;
    let connection_refs = load_connection_refs(pool, request.tenant_id)
        .await
        .map_err(moa_error_to_status_handler_error)?;
    build_capability_response(&registrations, &revisions, &connection_refs).map_err(|error| {
        TerminalError::new(format!(
            "failed to build execution capability catalog: {error}"
        ))
        .into()
    })
}

pub(crate) fn build_capability_response(
    registrations: &[(ToolDefinition, ToolExecution)],
    revisions: &[StoredArtifactRevision],
    connection_refs: &[String],
) -> moa_execution::Result<CapabilitiesListResponse> {
    let registered = registrations
        .iter()
        .map(|(definition, execution)| {
            (
                definition.name.clone(),
                (definition.clone(), execution.clone()),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut capabilities = registrations
        .iter()
        .map(|(definition, execution)| registered_tool_capability(definition, execution))
        .collect::<moa_execution::Result<Vec<_>>>()?;
    let mut diagnostics = connection_refs
        .iter()
        .map(|reference| CapabilityCatalogDiagnostic {
            code: CapabilityCatalogDiagnosticCode::ConnectionOnlyDataSource,
            reference: reference.clone(),
            message:
                "knowledge connections configure data access but have no typed invocation owner"
                    .to_string(),
        })
        .collect::<Vec<_>>();
    let mut artifact_action_bindings = Vec::new();
    for capability in &capabilities {
        if !matches!(
            &capability.source,
            CapabilitySource::InstalledConnectorAction { .. }
        ) {
            continue;
        }
        let Some(tool_name) = capability.source.model_visible_tool_name() else {
            continue;
        };
        let Some((definition, _)) = registered.get(tool_name) else {
            return Err(moa_execution::Error::InvalidProjection {
                message: format!(
                    "installed connector capability {} lost its typed registry owner",
                    capability.reference.name
                ),
            });
        };
        record_artifact_action_binding(&mut artifact_action_bindings, capability, definition)?;
    }

    for revision in revisions {
        match &revision.document.definition {
            ArtifactDefinition::Action(action) => {
                let action_ref = ArtifactRef::action_artifact(revision.name.clone());
                match resolve_tool(action.tool_name.as_deref(), &registered) {
                    Some((definition, execution)) => {
                        let capability = action_capability(ActionCapabilityRequest {
                            action_ref,
                            artifact_uid: revision.artifact_uid,
                            revision_uid: revision.revision_uid,
                            description: &action.description,
                            input_schema: &action.input_schema,
                            output_schema: &action.output_schema,
                            admin_review_required: action.admin_review_required,
                            definition,
                            execution,
                        })?;
                        record_artifact_action_binding(
                            &mut artifact_action_bindings,
                            &capability,
                            definition,
                        )?;
                        capabilities.push(capability);
                    }
                    None => diagnostics.push(unresolved_action_diagnostic(
                        action_ref.to_string(),
                        action.tool_name.as_deref(),
                    )),
                }
            }
            ArtifactDefinition::Connector(_) => {}
            ArtifactDefinition::Skill(_)
            | ArtifactDefinition::Agent(_)
            | ArtifactDefinition::ExperimentPlan(_) => {}
        }
    }

    for revision in revisions {
        let ArtifactDefinition::Skill(skill) = &revision.document.definition else {
            continue;
        };
        let skill_ref = ArtifactRef::artifact(ArtifactKind::Skill, revision.name.clone());
        for action in &skill.actions {
            append_skill_action(SkillActionContext {
                capabilities: &mut capabilities,
                diagnostics: &mut diagnostics,
                registered: &registered,
                artifact_action_bindings: &artifact_action_bindings,
                skill_ref: skill_ref.clone(),
                artifact_uid: revision.artifact_uid,
                revision_uid: revision.revision_uid,
                action,
            })?;
        }
    }

    diagnostics.sort_by(|left, right| {
        left.reference
            .cmp(&right.reference)
            .then_with(|| left.message.cmp(&right.message))
    });
    Ok(CapabilitiesListResponse {
        catalog: ExecutionCapabilityCatalog::build(capabilities)?,
        diagnostics,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ArtifactActionBinding {
    canonical_action_ref: ArtifactRef,
    tool_name: String,
    minimum_effect: ActionPolicyEffect,
}

pub(super) fn record_artifact_action_binding(
    bindings: &mut Vec<ArtifactActionBinding>,
    capability: &ExecutionCapability,
    definition: &ToolDefinition,
) -> moa_execution::Result<()> {
    let Some(canonical_action_ref) = capability.policy_context.canonical_action_ref.clone() else {
        return Err(moa_execution::Error::InvalidProjection {
            message: format!(
                "artifact capability {} has no canonical action reference",
                capability.reference.name
            ),
        });
    };
    let binding = ArtifactActionBinding {
        canonical_action_ref,
        tool_name: definition.name.clone(),
        minimum_effect: capability.policy_context.minimum_effect,
    };
    if let Some(existing) = bindings
        .iter_mut()
        .find(|existing| existing.canonical_action_ref == binding.canonical_action_ref)
    {
        if existing.tool_name != binding.tool_name {
            return Err(moa_execution::Error::InvalidProjection {
                message: format!(
                    "artifact action binding `{}` resolves to conflicting backing tools",
                    binding.canonical_action_ref
                ),
            });
        }
        existing.minimum_effect = stricter_effect(existing.minimum_effect, binding.minimum_effect);
    } else {
        bindings.push(binding);
    }
    Ok(())
}

/// Exact governed catalog and allowlist supplied to skill-regression compilation.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SkillRegressionCompileAuthority {
    /// Tenant-visible catalog built by the production execution catalog builder.
    pub catalog: ExecutionCapabilityCatalog,
    /// Exact capability and skill references authorized for this candidate review.
    pub authorization: moa_execution::ExecutionAuthorizationEnvelope,
}

/// Resolves compiler authority for one exact draft skill under its review scope.
pub(crate) async fn resolve_skill_regression_compile_authority(
    pool: sqlx::PgPool,
    registrations: Vec<(ToolDefinition, ToolExecution)>,
    scope: ActionRuleScope,
    draft: StoredArtifactRevision,
) -> MoaResult<SkillRegressionCompileAuthority> {
    if draft.kind != ArtifactKind::Skill || draft.status != ArtifactStatus::Draft {
        return Err(MoaError::ValidationError(
            "skill regression authority requires the exact draft skill revision".to_string(),
        ));
    }

    let registry = ArtifactRegistry::new(pool.clone());
    let mut revisions = load_serving_revisions(&registry, &scope).await?;
    let connection_refs = load_connection_refs(pool, scope.tenant_id()).await?;
    revisions.push(draft);
    build_skill_regression_compile_authority(&registrations, &revisions, &connection_refs)
}

pub(super) fn build_skill_regression_compile_authority(
    registrations: &[(ToolDefinition, ToolExecution)],
    revisions: &[StoredArtifactRevision],
    connection_refs: &[String],
) -> MoaResult<SkillRegressionCompileAuthority> {
    let response = build_capability_response(registrations, revisions, connection_refs)
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;

    let mut skill_refs = revisions
        .iter()
        .filter(|revision| matches!(revision.document.definition, ArtifactDefinition::Skill(_)))
        .map(|revision| ArtifactRef::artifact(ArtifactKind::Skill, revision.name.clone()))
        .collect::<Vec<_>>();
    skill_refs.sort_by_key(ToString::to_string);
    skill_refs.dedup();
    let authorization = moa_execution::ExecutionAuthorizationEnvelope {
        capability_refs: response
            .catalog
            .capabilities
            .iter()
            .map(|capability| capability.reference.clone())
            .collect(),
        skill_refs,
    };

    Ok(SkillRegressionCompileAuthority {
        catalog: response.catalog,
        authorization,
    })
}

pub(super) fn registered_tool_capability(
    definition: &ToolDefinition,
    execution: &ToolExecution,
) -> moa_execution::Result<ExecutionCapability> {
    let contract_revision = tool_contract_revision(definition, execution)?;
    let (source, execution_class, domain, owner) = match execution {
        ToolExecution::BuiltIn(_) if definition.name.starts_with("memory_") => (
            CapabilitySource::Memory {
                operation: definition
                    .name
                    .strip_prefix("memory_")
                    .unwrap_or(definition.name.as_str())
                    .to_string(),
                tool_name: definition.name.clone(),
            },
            ExecutionClass::Data,
            "moa.execution.capability.memory",
            json!({"kind": "memory"}),
        ),
        ToolExecution::BuiltIn(_) => (
            CapabilitySource::BuiltInTool {
                name: definition.name.clone(),
            },
            if definition.policy.action_class == moa_core::types::action_policy::ActionClass::Read {
                ExecutionClass::Data
            } else {
                ExecutionClass::Compute
            },
            "moa.execution.capability.builtin",
            json!({"kind": "builtin"}),
        ),
        ToolExecution::Hand { .. } => (
            CapabilitySource::HandTool {
                name: definition.name.clone(),
            },
            ExecutionClass::Compute,
            "moa.execution.capability.hand",
            json!({"kind": "hand"}),
        ),
        ToolExecution::Mcp {
            server_name,
            remote_tool_name,
            ..
        } => (
            // `definition.name` is the server-qualified reference; the source
            // records the name the server itself publishes, so provenance stays
            // answerable in the connector's own terms.
            CapabilitySource::McpTool {
                server: server_name.clone(),
                tool_name: definition.name.clone(),
                remote_name: remote_tool_name.clone(),
            },
            ExecutionClass::External,
            "moa.execution.capability.mcp",
            json!({"kind": "mcp", "server": server_name}),
        ),
        ToolExecution::InstalledConnectorAction {
            connector_ref,
            connection_id,
            binding_id,
            connection_generation,
            definition_artifact_uid,
            definition_revision_uid,
            action_id,
            contract_hash,
            governed_contract_revision,
            minimum_effect,
            ..
        } => {
            let connector_ref = connector_ref.parse::<ArtifactRef>().map_err(|error| {
                moa_execution::Error::InvalidProjection {
                    message: format!(
                        "installed connector tool {} has invalid logical reference: {error}",
                        definition.name
                    ),
                }
            })?;
            (
                CapabilitySource::InstalledConnectorAction {
                    connector_ref: connector_ref.clone(),
                    connection_id: *connection_id,
                    binding_id: binding_id.0,
                    connection_generation: connection_generation.get(),
                    definition_artifact_uid: *definition_artifact_uid,
                    definition_revision_uid: *definition_revision_uid,
                    action_id: action_id.clone(),
                    contract_hash: contract_hash.to_string(),
                    governed_contract_revision: contract_revision.clone(),
                    minimum_effect: *minimum_effect,
                    tool_name: definition.name.clone(),
                },
                ExecutionClass::External,
                "moa.execution.capability.installed-connector-action",
                json!({
                    "kind": "installed_connector_action",
                    "connector_ref": connector_ref,
                    "connection_id": connection_id,
                    "binding_id": binding_id,
                    "connection_generation": connection_generation,
                    "definition_artifact_uid": definition_artifact_uid,
                    "definition_revision_uid": definition_revision_uid,
                    "action_id": action_id,
                    "contract_hash": contract_hash,
                    "governed_contract_revision": governed_contract_revision,
                    "minimum_effect": minimum_effect,
                }),
            )
        }
    };
    let version = capability_version(
        domain,
        &json!({
            "name": definition.name,
            "input_schema": definition.schema,
            "policy": definition.policy,
            "idempotency_class": definition.idempotency_class,
            "max_output_tokens": definition.max_output_tokens,
            "owner": owner,
        }),
    )?;
    let reference_name = match &source {
        CapabilitySource::InstalledConnectorAction {
            connector_ref,
            action_id,
            ..
        } => ArtifactRef::action(connector_ref.target_name(), action_id).to_string(),
        _ => definition.name.clone(),
    };
    let policy_context = match &source {
        CapabilitySource::InstalledConnectorAction {
            connector_ref,
            definition_artifact_uid,
            definition_revision_uid,
            action_id,
            minimum_effect,
            ..
        } => CapabilityPolicyContext::artifact(
            source.clone(),
            Some(ArtifactRef::action(connector_ref.target_name(), action_id)),
            *definition_artifact_uid,
            *definition_revision_uid,
            *minimum_effect,
        ),
        _ => CapabilityPolicyContext::registered(source.clone()),
    };
    Ok(ExecutionCapability {
        reference: CapabilityReference {
            name: reference_name,
            version,
        },
        contract_revision,
        description: definition.description.clone(),
        input_schema: definition.schema.clone(),
        output_schema: generic_json_output_schema(),
        action_class: definition.policy.action_class,
        risk_level: definition.policy.risk_level,
        default_effect: definition.policy.default_effect,
        idempotency_class: definition.idempotency_class,
        execution_class,
        source,
        policy_context,
        estimate: single_tool_estimate(definition.max_output_tokens),
    })
}

pub(super) struct ActionCapabilityRequest<'a> {
    action_ref: ArtifactRef,
    artifact_uid: uuid::Uuid,
    revision_uid: uuid::Uuid,
    description: &'a str,
    input_schema: &'a Value,
    output_schema: &'a Value,
    admin_review_required: bool,
    definition: &'a ToolDefinition,
    execution: &'a ToolExecution,
}

pub(super) fn action_capability(
    request: ActionCapabilityRequest<'_>,
) -> moa_execution::Result<ExecutionCapability> {
    let ActionCapabilityRequest {
        action_ref,
        artifact_uid,
        revision_uid,
        description,
        input_schema,
        output_schema,
        admin_review_required,
        definition,
        execution,
    } = request;
    let source = CapabilitySource::ActionArtifact {
        action_ref: action_ref.clone(),
        revision_uid,
        tool_name: definition.name.clone(),
    };
    let policy_context = CapabilityPolicyContext::artifact(
        source.clone(),
        Some(action_ref.clone()),
        artifact_uid,
        revision_uid,
        artifact_minimum_effect(admin_review_required),
    );
    Ok(ExecutionCapability {
        reference: CapabilityReference {
            name: action_ref.to_string(),
            version: revision_uid.to_string(),
        },
        contract_revision: tool_contract_revision(definition, execution)?,
        description: description.to_string(),
        input_schema: input_schema.clone(),
        output_schema: output_schema.clone(),
        action_class: definition.policy.action_class,
        risk_level: definition.policy.risk_level,
        default_effect: artifact_effect(admin_review_required, &definition.policy),
        idempotency_class: definition.idempotency_class,
        execution_class: execution_class(execution, definition),
        source,
        policy_context,
        estimate: single_tool_estimate(definition.max_output_tokens),
    })
}

pub(super) struct SkillActionContext<'a> {
    capabilities: &'a mut Vec<ExecutionCapability>,
    diagnostics: &'a mut Vec<CapabilityCatalogDiagnostic>,
    registered: &'a HashMap<String, (ToolDefinition, ToolExecution)>,
    artifact_action_bindings: &'a [ArtifactActionBinding],
    skill_ref: ArtifactRef,
    artifact_uid: uuid::Uuid,
    revision_uid: uuid::Uuid,
    action: &'a SkillActionDefinition,
}

pub(super) fn append_skill_action(context: SkillActionContext<'_>) -> moa_execution::Result<()> {
    let SkillActionContext {
        capabilities,
        diagnostics,
        registered,
        artifact_action_bindings,
        skill_ref,
        artifact_uid,
        revision_uid,
        action,
    } = context;
    let reference = format!("{skill_ref}#{}", action.id);
    if action.kind == SkillActionKind::Code {
        diagnostics.push(CapabilityCatalogDiagnostic {
            code: CapabilityCatalogDiagnosticCode::UnownedSkillCode,
            reference,
            message: "skill code has no registered typed execution owner".to_string(),
        });
        return Ok(());
    }
    let inherited_binding = action.artifact_ref.as_ref().and_then(|artifact_ref| {
        artifact_action_bindings
            .iter()
            .find(|binding| binding.canonical_action_ref == *artifact_ref)
    });
    let tool_name = inherited_binding
        .map(|binding| binding.tool_name.as_str())
        .or(match action.artifact_ref.as_ref() {
            Some(ArtifactRef::Tool { name }) => Some(name.as_str()),
            Some(ArtifactRef::Artifact { .. } | ArtifactRef::Action { .. }) | None => None,
        });
    let Some((definition, execution)) = resolve_tool(tool_name, registered) else {
        diagnostics.push(unresolved_action_diagnostic(reference, tool_name));
        return Ok(());
    };
    if matches!(execution, ToolExecution::InstalledConnectorAction { .. }) {
        let mut capability = registered_tool_capability(definition, execution)?;
        capability.reference = CapabilityReference {
            name: reference,
            version: revision_uid.to_string(),
        };
        capability.description = action.description.clone();
        capability.input_schema = action.input_schema.clone();
        capability.output_schema = action.output_schema.clone();
        // The skill alias changes presentation and authorization naming only.
        // Durable execution provenance remains the exact installed connection
        // source and definition policy context, so replay never has to recover
        // connection authority from the generated model-visible tool name.
        capabilities.push(capability);
        return Ok(());
    }
    let canonical_action_ref =
        inherited_binding.map(|binding| binding.canonical_action_ref.clone());
    let minimum_effect =
        inherited_binding.map_or(ActionPolicyEffect::Allow, |binding| binding.minimum_effect);
    let source = CapabilitySource::SkillAction {
        skill_ref,
        revision_uid,
        action_id: action.id.clone(),
        tool_name: definition.name.clone(),
    };
    let policy_context = CapabilityPolicyContext::artifact(
        source.clone(),
        canonical_action_ref,
        artifact_uid,
        revision_uid,
        minimum_effect,
    );
    capabilities.push(ExecutionCapability {
        reference: CapabilityReference {
            name: reference,
            version: revision_uid.to_string(),
        },
        contract_revision: tool_contract_revision(definition, execution)?,
        description: action.description.clone(),
        input_schema: action.input_schema.clone(),
        output_schema: action.output_schema.clone(),
        action_class: definition.policy.action_class,
        risk_level: definition.policy.risk_level,
        default_effect: definition.policy.default_effect,
        idempotency_class: definition.idempotency_class,
        execution_class: execution_class(execution, definition),
        source,
        policy_context,
        estimate: single_tool_estimate(definition.max_output_tokens),
    });
    Ok(())
}

pub(super) fn tool_contract_revision(
    definition: &ToolDefinition,
    execution: &ToolExecution,
) -> moa_execution::Result<String> {
    moa_hands::governed_tool_contract_revision(definition, execution).map_err(|error| {
        moa_execution::Error::InvalidProjection {
            message: format!(
                "failed to pin governed contract for tool {}: {error}",
                definition.name
            ),
        }
    })
}

pub(super) fn resolve_tool<'a>(
    tool_name: Option<&str>,
    registered: &'a HashMap<String, (ToolDefinition, ToolExecution)>,
) -> Option<(&'a ToolDefinition, &'a ToolExecution)> {
    let (definition, execution) = registered.get(tool_name?)?;
    Some((definition, execution))
}

pub(super) fn unresolved_action_diagnostic(
    reference: String,
    tool_name: Option<&str>,
) -> CapabilityCatalogDiagnostic {
    CapabilityCatalogDiagnostic {
        code: CapabilityCatalogDiagnosticCode::UnresolvedActionTool,
        reference,
        message: tool_name.map_or_else(
            || "action does not declare a backing tool".to_string(),
            |name| format!("action backing tool `{name}` is not registered"),
        ),
    }
}

pub(super) fn artifact_effect(
    admin_review_required: bool,
    policy: &ToolPolicySpec,
) -> ActionPolicyEffect {
    if admin_review_required {
        ActionPolicyEffect::AdminReview
    } else {
        policy.default_effect
    }
}

pub(super) fn artifact_minimum_effect(admin_review_required: bool) -> ActionPolicyEffect {
    if admin_review_required {
        ActionPolicyEffect::AdminReview
    } else {
        ActionPolicyEffect::Allow
    }
}

pub(super) fn execution_class(
    execution: &ToolExecution,
    definition: &ToolDefinition,
) -> ExecutionClass {
    match execution {
        ToolExecution::Hand { .. } => ExecutionClass::Compute,
        ToolExecution::Mcp { .. } | ToolExecution::InstalledConnectorAction { .. } => {
            ExecutionClass::External
        }
        ToolExecution::BuiltIn(_)
            if definition.policy.action_class
                == moa_core::types::action_policy::ActionClass::Read =>
        {
            ExecutionClass::Data
        }
        ToolExecution::BuiltIn(_) => ExecutionClass::Compute,
    }
}

pub(super) fn single_tool_estimate(max_output_tokens: u32) -> ExecutionEstimate {
    ExecutionEstimate {
        tool_calls: 1,
        tasks: 1,
        // Tool output budgeting uses a conservative four-characters-per-token
        // approximation. UTF-8 and JSON escaping can expand each character to
        // at most four bytes in the structured payload retained for execution.
        retrieved_bytes: u64::from(max_output_tokens).saturating_mul(16).max(4),
        ..ExecutionEstimate::default()
    }
}

pub(super) fn generic_json_output_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "description": "The complete JSON value returned by the registered tool."
    })
}

/// Loads the artifact revisions a tenant actually serves for plan compilation.
///
/// Skills and actions come from their type-owned serving pointers. Connectors
/// still come from validated `published` revisions because connector catalog
/// activation is platform-owned rather than part of the artifact release gate.
pub(super) async fn load_serving_revisions(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
) -> MoaResult<Vec<StoredArtifactRevision>> {
    let mut revisions = Vec::new();
    for summary in registry
        .list_visible(
            scope,
            Some(ArtifactKind::Connector),
            Some(ArtifactStatus::Published),
        )
        .await?
    {
        if let Some(revision) = registry.load_revision(scope, summary.revision_uid).await? {
            revisions.push(revision);
        }
    }
    for kind in [ArtifactKind::Action, ArtifactKind::Skill] {
        for summary in registry.list_serving(scope, kind).await? {
            if let Some(revision) = registry.load_revision(scope, summary.revision_uid).await? {
                revisions.push(revision);
            }
        }
    }
    Ok(revisions)
}

/// Loads the exact revisions used by the production capability loader.
///
/// This integration-only seam exists so the PostgreSQL lane can pin serving
/// pointer semantics without copying the loader query into the test.
#[cfg(feature = "integration")]
pub async fn load_serving_revisions_for_test(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
) -> MoaResult<Vec<StoredArtifactRevision>> {
    load_serving_revisions(registry, scope).await
}

pub(super) async fn load_locked_skill_revisions(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    agent_context: Option<&moa_core::types::agent::AgentContext>,
) -> Result<Vec<StoredArtifactRevision>, HandlerError> {
    let Some(agent_context) = agent_context else {
        return Ok(Vec::new());
    };
    let mut dependencies = agent_context
        .artifact_dependencies
        .iter()
        .filter(|dependency| dependency.kind == "skill")
        .collect::<Vec<_>>();
    dependencies.sort_by(|left, right| {
        left.reference
            .cmp(&right.reference)
            .then_with(|| left.revision_uid.cmp(&right.revision_uid))
    });

    let mut revisions = Vec::with_capacity(dependencies.len());
    let release_scope = TenantScope::new(scope.tenant_id());
    for dependency in dependencies {
        let canonical_ref =
            canonical_skill_policy_ref(&dependency.reference).map_err(invalid_execution_request)?;
        let revision = registry
            .load_revision(scope, dependency.revision_uid)
            .await
            .map_err(moa_error_to_status_handler_error)?
            .ok_or_else(|| {
                invalid_execution_request(format!(
                    "session skill lock revision is not loadable: {}",
                    dependency.revision_uid
                ))
            })?;
        let revision_ref = skill_revision_ref(&revision).map_err(invalid_execution_request)?;
        if !registry
            .was_ever_activated(&release_scope, revision.revision_uid)
            .await
            .map_err(moa_error_to_status_handler_error)?
        {
            return Err(invalid_execution_request(format!(
                "session skill lock revision was never activated: {}",
                dependency.revision_uid
            )));
        }
        if revision_ref != canonical_ref
            || revision.name != dependency.name
            || revision.artifact_uid != dependency.artifact_uid
            || revision.revision_uid != dependency.revision_uid
            || revision.version != dependency.version
        {
            return Err(invalid_execution_request(format!(
                "session skill lock does not match persisted revision: {}",
                dependency.reference
            )));
        }
        revisions.push(revision);
    }
    Ok(revisions)
}

pub(super) async fn load_connection_refs(
    pool: sqlx::PgPool,
    tenant_id: moa_core::types::identifiers::TenantId,
) -> MoaResult<Vec<String>> {
    PostgresKnowledgeRepository::scoped(pool, RlsContext::tenant(tenant_id))
        .list_connections(tenant_id, None)
        .await
        .map(|connections| {
            connections
                .into_iter()
                .map(|projection| projection.connection.connection_uid.to_string())
                .collect()
        })
        .map_err(|error| {
            tracing::error!(error = %error, "execution capability connection listing failed");
            MoaError::StorageError("failed to inspect knowledge connections".to_string())
        })
}
