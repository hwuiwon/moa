//! MCP connector health, staged catalog activation, and background refresh.
//!
//! A connector's tools enter the router through one path only: a discovery pass
//! that produces a whole new catalog snapshot and publishes it atomically. That
//! shape is what lets a single connector fail without touching anyone else's
//! tools, lets a refresh keep serving a connector's last-known-good tools while
//! it is down, and guarantees no reader ever sees a half-applied refresh.
//!
//! Discovery does not *replace* the active catalog. It stages a candidate keyed
//! by connector revision and schema hash, runs deterministic structural and
//! policy checks over it, and only then activates it. A
//! candidate connector that fails a deterministic check is quarantined and the
//! last-known-good tools stay active, because withdrawing a working integration
//! is itself an outage.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use moa_config::{McpServerConfig, MoaConfig};
use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionPolicyEffect,
    types::identifiers::ConnectorConnectionId,
};
use moa_security::{ConnectorCandidateFacts, ConnectorPolicyDefect, check_connector_policy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::adapters::mcp::{MCPClient, McpDiscoveredToolRegistration};

use super::registration::{
    McpClientRoute, McpClientRouteState, ToolExecution, governed_tool_contract_revision,
};
use super::{ToolCatalogSnapshot, ToolRegistry, ToolRouter};

/// Absorbs one length-framed component into a digest.
///
/// Framing every component is what keeps the digests in this module injective:
/// without it, moving a byte across a boundary between two caller-controlled
/// strings would produce the same hash for two different catalogs.
fn absorb(hasher: &mut Sha256, part: &[u8]) {
    hasher.update((part.len() as u64).to_be_bytes());
    hasher.update(part);
}

/// Typed discovery health for one configured MCP connector.
///
/// Health is per connector, never aggregated into a single router-wide flag: an
/// aggregate cannot express "this optional integration is down and every other
/// tool is fine", which is exactly the state the router has to be able to serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpConnectorHealth {
    /// Configured with lazy discovery and not yet discovered.
    ///
    /// Distinct from `Unavailable`: nothing has been attempted, so nothing has
    /// failed. Collapsing the two would report a healthy deployment as broken
    /// for as long as its lazy connectors stayed unused.
    Pending,
    /// Discovered successfully; its tools are the ones currently served.
    Ready {
        /// Number of tools this connector contributes to the catalog.
        tools: usize,
        /// Fixed protocol revision used during successful discovery.
        protocol_version: String,
        /// When the successful discovery completed.
        observed_at: DateTime<Utc>,
    },
    /// Discovery failed but a previous success is still being served.
    ///
    /// The retained tools are last-known-good, not a guess: they were observed
    /// from this same connector and are still the schemas it published. Dropping
    /// them on one failed refresh would make an unrelated transient error
    /// silently shrink the model's loadout.
    Degraded {
        /// Number of last-known-good tools still served for this connector.
        tools: usize,
        /// When the retained tools were last discovered successfully.
        last_good_at: DateTime<Utc>,
        /// Why the most recent discovery attempt failed.
        error: String,
        /// When the failed attempt happened.
        observed_at: DateTime<Utc>,
    },
    /// Discovery succeeded but the candidate failed a deterministic check.
    ///
    /// Distinct from `Degraded`, which means "we could not reach it". Here the
    /// connector answered and what it published was rejected, so retrying the
    /// transport will not help and the report has to name the exact defects.
    /// Whatever this connector was already serving keeps serving, because a
    /// malformed candidate is not a reason to withdraw a contract that works.
    Quarantined {
        /// Number of last-known-good tools still served for this connector.
        tools: usize,
        /// When the retained tools were last discovered successfully, if ever.
        last_good_at: Option<DateTime<Utc>>,
        /// Every deterministic defect the candidate failed on.
        defects: Vec<CatalogDefect>,
        /// When the rejected candidate was staged.
        observed_at: DateTime<Utc>,
    },
    /// Discovery failed with no previous success to fall back on.
    Unavailable {
        /// Why discovery failed.
        error: String,
        /// When the failed attempt happened.
        observed_at: DateTime<Utc>,
    },
}

impl McpConnectorHealth {
    /// Returns whether this connector is currently serving discovered tools.
    #[must_use]
    pub fn serves_tools(&self) -> bool {
        match self {
            Self::Ready { .. } | Self::Degraded { .. } => true,
            Self::Quarantined { tools, .. } => *tools > 0,
            Self::Pending | Self::Unavailable { .. } => false,
        }
    }

    /// Returns the stable machine-readable state label.
    #[must_use]
    pub fn state(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready { .. } => "ready",
            Self::Degraded { .. } => "degraded",
            Self::Quarantined { .. } => "quarantined",
            Self::Unavailable { .. } => "unavailable",
        }
    }

    /// Returns the failure detail, when this connector's last attempt failed.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Degraded { error, .. } | Self::Unavailable { error, .. } => Some(error.as_str()),
            Self::Quarantined { defects, .. } => defects.first().map(CatalogDefect::detail),
            Self::Pending | Self::Ready { .. } => None,
        }
    }

    /// Returns the last successful discovery time this state carries, if any.
    fn last_good_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Ready { observed_at, .. } => Some(*observed_at),
            Self::Degraded { last_good_at, .. } => Some(*last_good_at),
            Self::Quarantined { last_good_at, .. } => *last_good_at,
            Self::Pending | Self::Unavailable { .. } => None,
        }
    }

    /// Renders the startup failure a required connector in this state produces.
    fn required_startup_failure(&self, server_name: &str) -> MoaError {
        MoaError::ConfigError(format!(
            "required MCP connector '{server_name}' is {}: {}",
            self.state(),
            self.error().unwrap_or("discovery has not run"),
        ))
    }
}

/// One deterministic reason a candidate connector was quarantined.
///
/// Every variant is reproducible from the candidate itself: a transport error,
/// a declared-policy violation, or a registration rejection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogDefect {
    /// The connector could not be reached or did not complete protocol discovery.
    Discovery {
        /// Transport or protocol failure detail.
        error: String,
    },
    /// The connector violated a declared deployment security policy.
    Policy {
        /// The exact deterministic policy violation.
        violation: ConnectorPolicyDefect,
    },
    /// Every tool the connector published was unofferable to a model.
    ///
    /// One unofferable tool among many is a warning — the rest of the connector
    /// still works. All of them failing means the connector contributes nothing,
    /// which is a candidate that cannot be activated rather than a partial one.
    NoOfferableTools {
        /// How many published tools were rejected at registration.
        rejected: usize,
        /// Why the first rejected tool could not be offered.
        first_error: String,
    },
}

impl CatalogDefect {
    /// Returns the human-readable detail for logs and health reporting.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Discovery { error } => error.as_str(),
            Self::Policy { .. } | Self::NoOfferableTools { .. } => self.kind(),
        }
    }

    /// Returns the stable machine-readable defect kind.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Discovery { .. } => "discovery",
            Self::Policy { .. } => "policy",
            Self::NoOfferableTools { .. } => "no_offerable_tools",
        }
    }
}

impl std::fmt::Display for CatalogDefect {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Discovery { error } => write!(formatter, "discovery failed: {error}"),
            Self::Policy { violation } => write!(formatter, "policy violation: {violation}"),
            Self::NoOfferableTools {
                rejected,
                first_error,
            } if *rejected == 0 => write!(formatter, "connector published no tools: {first_error}"),
            Self::NoOfferableTools {
                rejected,
                first_error,
            } => write!(
                formatter,
                "all {rejected} published tools were unofferable: {first_error}"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool schema pin
// ---------------------------------------------------------------------------

/// Namespace that owns one pinned tool schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "owner", rename_all = "snake_case")]
pub enum PinnedToolOwner {
    /// An in-process built-in tool.
    BuiltIn,
    /// A sandbox-executed tool, independent of the serving provider.
    Hand,
    /// A tool published by one configured MCP connector.
    Connector {
        /// Configured connector name.
        server: String,
    },
    /// An action on one exact tenant-installed connection and binding revision.
    InstalledConnectorAction {
        /// Canonical logical connector reference selected by the agent.
        connector_ref: String,
        /// Exact tenant connection identity.
        connection_id: ConnectorConnectionId,
        /// Exact immutable installed binding row.
        binding_id: uuid::Uuid,
        /// Positive connection generation that compiled the binding.
        connection_generation: u64,
        /// Published connector artifact family.
        definition_artifact_uid: uuid::Uuid,
        /// Exact published connector artifact revision.
        definition_revision_uid: uuid::Uuid,
        /// Definition-local canonical action identifier.
        action_id: String,
        /// Canonical normalized operation-contract hash.
        contract_hash: String,
        /// Governed action revision persisted with the binding.
        governed_contract_revision: String,
        /// Intrinsic effect floor that persisted rules cannot lower.
        minimum_effect: ActionPolicyEffect,
    },
}

/// One tool's exact governed contract inside an activated catalog snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedToolContract {
    /// Registered tool name, which for a connector tool is its qualified reference.
    pub tool: String,
    /// Namespace that owns the tool.
    pub owner: PinnedToolOwner,
    /// Digest of the complete governed execution contract at activation time.
    pub contract_revision: String,
}

/// The exact governed tool-contract snapshot one activated catalog serves.
///
/// Conversational turns carry this beside the model-visible schemas, and
/// durable execution capabilities retain each invoked tool's contract revision.
/// It covers every registered tool, not only connector tools, so built-in and
/// hand contract changes are visible across deployments too.
///
/// `contract_hash` identifies the whole snapshot for verification and
/// observability. Invocation paths compare the selected tool's
/// `contract_revision`, so an unrelated catalog update does not invalidate a
/// still-current call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCatalogPin {
    /// Digest over every pinned tool's name, owner, and governed contract revision.
    pub contract_hash: String,
    /// Revision of the connector portion of the same snapshot.
    ///
    /// Retained separately because connector schemas are the part that moves
    /// while a process keeps running, so operators and dashboards need to see
    /// connector churn without diffing the whole catalog.
    pub mcp_catalog_revision: String,
    /// Every pinned tool in stable name order.
    pub tools: Vec<PinnedToolContract>,
}

impl ToolCatalogPin {
    /// Builds the pin for one registry snapshot.
    pub(super) fn from_registry(registry: &ToolRegistry) -> Result<Self> {
        let mut tools = Vec::new();
        for (definition, execution) in registry.capability_registrations() {
            let owner = match &execution {
                ToolExecution::BuiltIn(_) => PinnedToolOwner::BuiltIn,
                ToolExecution::Hand { .. } => PinnedToolOwner::Hand,
                ToolExecution::Mcp { server_name, .. } => PinnedToolOwner::Connector {
                    server: server_name.clone(),
                },
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
                } => PinnedToolOwner::InstalledConnectorAction {
                    connector_ref: connector_ref.clone(),
                    connection_id: *connection_id,
                    binding_id: binding_id.0,
                    connection_generation: connection_generation.get(),
                    definition_artifact_uid: *definition_artifact_uid,
                    definition_revision_uid: *definition_revision_uid,
                    action_id: action_id.clone(),
                    contract_hash: contract_hash.to_string(),
                    governed_contract_revision: governed_contract_revision.clone(),
                    minimum_effect: *minimum_effect,
                },
            };
            let contract_revision = governed_tool_contract_revision(&definition, &execution)?;
            tools.push(PinnedToolContract {
                tool: definition.name,
                owner,
                contract_revision,
            });
        }
        // `capability_registrations` is already name-sorted; the sort here keeps
        // that a property of this function rather than of a call it makes.
        tools.sort_by(|left, right| left.tool.cmp(&right.tool));

        let mut hasher = Sha256::new();
        hasher.update(b"moa.tool.catalog-pin.v1");
        for tool in &tools {
            absorb(&mut hasher, tool.tool.as_bytes());
            absorb(
                &mut hasher,
                match &tool.owner {
                    PinnedToolOwner::BuiltIn => "builtin",
                    PinnedToolOwner::Hand => "hand",
                    PinnedToolOwner::Connector { .. } => "connector",
                    PinnedToolOwner::InstalledConnectorAction { .. } => {
                        "installed_connector_action"
                    }
                }
                .as_bytes(),
            );
            absorb(
                &mut hasher,
                match &tool.owner {
                    PinnedToolOwner::Connector { server } => server.as_str(),
                    PinnedToolOwner::InstalledConnectorAction { connector_ref, .. } => {
                        connector_ref.as_str()
                    }
                    PinnedToolOwner::BuiltIn | PinnedToolOwner::Hand => "",
                }
                .as_bytes(),
            );
            if let PinnedToolOwner::InstalledConnectorAction {
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
            } = &tool.owner
            {
                absorb(&mut hasher, connection_id.0.as_bytes());
                absorb(&mut hasher, binding_id.as_bytes());
                absorb(&mut hasher, &connection_generation.to_be_bytes());
                absorb(&mut hasher, definition_artifact_uid.as_bytes());
                absorb(&mut hasher, definition_revision_uid.as_bytes());
                absorb(&mut hasher, action_id.as_bytes());
                absorb(&mut hasher, contract_hash.as_bytes());
                absorb(&mut hasher, governed_contract_revision.as_bytes());
                absorb(&mut hasher, minimum_effect.as_str().as_bytes());
            }
            absorb(&mut hasher, tool.contract_revision.as_bytes());
        }

        Ok(Self {
            contract_hash: hex::encode(hasher.finalize()),
            mcp_catalog_revision: mcp_catalog_revision(registry),
            tools,
        })
    }

    /// Returns the pinned governed contract revision of one tool.
    #[must_use]
    pub fn contract_revision(&self, tool: &str) -> Option<&str> {
        self.tools
            .iter()
            .find(|pinned| pinned.tool == tool)
            .map(|pinned| pinned.contract_revision.as_str())
    }
}

/// One difference between a pinned snapshot and the activated one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "drift", rename_all = "snake_case")]
pub enum ToolCatalogDrift {
    /// A pinned tool is no longer served.
    Withdrawn {
        /// Registered tool name.
        tool: String,
    },
    /// A tool's governed contract changed.
    ContractMoved {
        /// Registered tool name.
        tool: String,
        /// Governed contract revision recorded in this pin.
        pinned_revision: String,
        /// Governed contract revision in the activated snapshot.
        activated_revision: String,
    },
}

// ---------------------------------------------------------------------------
// Candidate staging and activation
// ---------------------------------------------------------------------------

/// One candidate connector keyed by its revision and schema hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateConnector {
    /// Digest over the connector's configured identity and fixed protocol revision.
    pub connector_revision: String,
    /// Digest over every tool schema the candidate published.
    pub schema_hash: String,
    /// Fixed protocol revision used for this candidate.
    pub protocol_version: String,
    /// Number of tools the candidate offers.
    pub tools: usize,
}

/// Outcome of one staged catalog activation.
#[derive(Clone, Debug)]
pub struct McpCatalogActivation {
    /// The exact governed tool-contract snapshot that is now activated.
    pub pin: ToolCatalogPin,
    /// Candidate connectors that cleared every deterministic check.
    pub activated: BTreeMap<String, CandidateConnector>,
    /// Candidate connectors rejected, with every defect that rejected them.
    pub quarantined: BTreeMap<String, Vec<CatalogDefect>>,
    /// Non-blocking observations recorded alongside the activation.
    pub warnings: BTreeMap<String, Vec<String>>,
}

/// Outcome of one catalog discovery pass.
#[derive(Clone, Debug)]
pub struct McpCatalogRefresh {
    /// Typed health for every configured connector after the pass.
    pub health: BTreeMap<String, McpConnectorHealth>,
    /// Catalog revision published by the pass.
    pub revision: String,
    /// The whole activation record, including quarantines and warnings.
    pub activation: McpCatalogActivation,
}

impl ToolRouter {
    /// Returns the typed health of every configured MCP connector.
    pub async fn mcp_connector_health(&self) -> BTreeMap<String, McpConnectorHealth> {
        self.mcp.health_snapshot().await
    }

    /// Returns the revision identifying the MCP portion of the live catalog.
    ///
    /// Two routers that discovered the same connectors at the same schemas
    /// publish the same revision, and any change to a served tool's identity or
    /// schema changes it. It is the value a caller pins when it needs to know
    /// that the schemas it compiled into a prompt are the schemas the router is
    /// still serving.
    #[must_use]
    pub fn mcp_catalog_revision(&self) -> String {
        self.catalog_pin()
            .map(|pin| pin.mcp_catalog_revision)
            .unwrap_or_else(|_| mcp_catalog_revision(&self.registry()))
    }

    /// Discovers every eagerly configured connector and publishes one catalog.
    ///
    /// Called once while the router is being built. A required connector that
    /// does not reach [`McpConnectorHealth::Ready`] fails startup with its typed
    /// health; an optional one is recorded and skipped, and its absence removes
    /// only its own tools.
    pub(super) async fn load_mcp_catalog(&self, config: &MoaConfig) -> Result<()> {
        let refresh = self
            .run_mcp_discovery(&config.mcp_servers, DiscoveryPass::Startup)
            .await?;
        for server in &config.mcp_servers {
            if !server.required {
                continue;
            }
            let health = refresh
                .health
                .get(&server.name)
                .cloned()
                .unwrap_or(McpConnectorHealth::Pending);
            if !matches!(health, McpConnectorHealth::Ready { .. }) {
                return Err(health.required_startup_failure(&server.name));
            }
        }
        Ok(())
    }

    /// Re-discovers every configured connector and republishes the catalog.
    ///
    /// Safe to call on a serving router: candidates are staged and validated off
    /// to the side and the accepted set is swapped in as a whole. A connector
    /// that fails — whether it could not be reached or published something that
    /// failed a deterministic check — keeps serving its last-known-good tools
    /// rather than losing them to a transient error or a bad deploy.
    pub async fn refresh_mcp_catalog(&self) -> McpCatalogRefresh {
        let servers = self.configured_mcp_servers();
        let refresh = match self
            .run_mcp_discovery(&servers, DiscoveryPass::Refresh)
            .await
        {
            Ok(refresh) => refresh,
            Err(error) => {
                // Pinning is pure computation over an already-published
                // registry; it can only fail if a tool schema stopped being
                // canonicalizable, which is a deployment bug rather than a
                // connector outage. Reporting it as such beats aborting the
                // refresh loop.
                tracing::error!(
                    %error,
                    "MCP catalog refresh could not pin the activated snapshot"
                );
                let health = self.mcp.health_snapshot().await;
                let revision = self.mcp_catalog_revision();
                McpCatalogRefresh {
                    health: health.clone(),
                    revision: revision.clone(),
                    activation: McpCatalogActivation {
                        pin: ToolCatalogPin {
                            contract_hash: String::new(),
                            mcp_catalog_revision: revision,
                            tools: Vec::new(),
                        },
                        activated: BTreeMap::new(),
                        quarantined: BTreeMap::new(),
                        warnings: BTreeMap::new(),
                    },
                }
            }
        };
        self.mcp.complete_refresh_request();
        refresh
    }

    /// Returns the configured connectors in their authored order.
    fn configured_mcp_servers(&self) -> Vec<McpServerConfig> {
        self.mcp.configured_servers()
    }

    async fn run_mcp_discovery(
        &self,
        servers: &[McpServerConfig],
        pass: DiscoveryPass,
    ) -> Result<McpCatalogRefresh> {
        // A refresh must not count its own read as a stale-catalog access.
        let mut registry = (*self.catalog.activated().registry).clone();
        let mut health = self.mcp.health_snapshot().await;
        let mut activated = BTreeMap::new();
        let mut quarantined: BTreeMap<String, Vec<CatalogDefect>> = BTreeMap::new();
        let mut warnings: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut catalog_fresh_until: Option<Instant> = None;
        let mut serving_stale_tools = false;

        for server in servers {
            if pass.defers(server) {
                health
                    .entry(server.name.clone())
                    .or_insert(McpConnectorHealth::Pending);
                continue;
            }
            let observed_at = Utc::now();
            let staged = self.stage_connector(server).await;
            match staged {
                Ok(candidate) => {
                    catalog_fresh_until = Some(
                        catalog_fresh_until.map_or(candidate.catalog_fresh_until, |current| {
                            current.min(candidate.catalog_fresh_until)
                        }),
                    );
                    if !candidate.warnings.is_empty() {
                        for warning in &candidate.warnings {
                            tracing::warn!(
                                mcp_server = %server.name,
                                warning = %warning,
                                "MCP catalog candidate activated with a non-blocking warning"
                            );
                        }
                        warnings.insert(server.name.clone(), candidate.warnings.clone());
                    }
                    // Only now does the candidate touch the served catalog: the
                    // connector's previous tools are replaced as a unit by an
                    // already-validated set.
                    registry.remove_mcp_server_tools(&server.name);
                    let client_route: McpClientRoute = Arc::new(tokio::sync::Mutex::new(
                        McpClientRouteState::with_client(Arc::clone(&candidate.client)),
                    ));
                    let mut registered = 0_usize;
                    for tool in candidate.tools {
                        match registry.register_mcp_tool_on_route(
                            &server.name,
                            tool,
                            Arc::clone(&client_route),
                        ) {
                            Ok(_) => registered += 1,
                            Err(error) => tracing::warn!(
                                mcp_server = %server.name,
                                %error,
                                "skipped an MCP tool that cannot be offered to a model"
                            ),
                        }
                    }
                    activated.insert(
                        server.name.clone(),
                        CandidateConnector {
                            connector_revision: candidate.connector_revision,
                            schema_hash: candidate.schema_hash,
                            protocol_version: candidate.protocol_version.clone(),
                            tools: registered,
                        },
                    );
                    health.insert(
                        server.name.clone(),
                        McpConnectorHealth::Ready {
                            tools: registered,
                            protocol_version: candidate.protocol_version,
                            observed_at,
                        },
                    );
                }
                Err(defects) => {
                    let retained = retained_tool_count(&registry, &server.name);
                    serving_stale_tools |= retained > 0;
                    let last_good_at = health
                        .get(&server.name)
                        .and_then(McpConnectorHealth::last_good_at);
                    let unreachable = defects.iter().find_map(|defect| match defect {
                        CatalogDefect::Discovery { error } => Some(error.clone()),
                        _ => None,
                    });
                    let next = match (unreachable, last_good_at.filter(|_| retained > 0)) {
                        // Transport failure with last-known-good tools: the
                        // existing degraded contract, unchanged.
                        (Some(error), Some(last_good_at)) => McpConnectorHealth::Degraded {
                            tools: retained,
                            last_good_at,
                            error,
                            observed_at,
                        },
                        (Some(error), None) => {
                            registry.remove_mcp_server_tools(&server.name);
                            McpConnectorHealth::Unavailable { error, observed_at }
                        }
                        // The connector answered and its candidate was rejected.
                        // Whatever it was serving keeps serving: a malformed
                        // candidate is not a reason to withdraw a working
                        // contract.
                        (None, last_good_at) => McpConnectorHealth::Quarantined {
                            tools: retained,
                            last_good_at,
                            defects: defects.clone(),
                            observed_at,
                        },
                    };
                    tracing::warn!(
                        mcp_server = %server.name,
                        required = server.required,
                        health = next.state(),
                        defects = defects.len(),
                        detail = %defects
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("; "),
                        "MCP connector candidate was not activated"
                    );
                    quarantined.insert(server.name.clone(), defects);
                    health.insert(server.name.clone(), next);
                }
            }
        }

        registry.apply_budgets(self.bindings.tool_budgets());
        let revision = mcp_catalog_revision(&registry);
        let snapshot = ToolCatalogSnapshot::new(self.catalog.owner_id(), registry);
        let pin = snapshot.pin()?;
        self.publish_catalog_snapshot(snapshot);
        self.refresh_unmatched_permission_patterns();
        self.mcp.publish_health(health.clone()).await;
        self.mcp
            .publish_catalog_fresh_until(if serving_stale_tools {
                Some(Instant::now())
            } else {
                catalog_fresh_until
            });
        Ok(McpCatalogRefresh {
            health: health.clone(),
            revision,
            activation: McpCatalogActivation {
                pin,
                activated,
                quarantined,
                warnings,
            },
        })
    }

    /// Discovers one connector and runs every deterministic check over the
    /// candidate it produced.
    ///
    /// Returns the candidate only when every check held; otherwise it returns
    /// every defect, so one staging pass tells an operator everything that has
    /// to change rather than one problem at a time.
    async fn stage_connector(
        &self,
        server: &McpServerConfig,
    ) -> std::result::Result<StagedConnector, Vec<CatalogDefect>> {
        let discovered = match self.discover_server_tools(server).await {
            Ok(discovered) => discovered,
            Err(error) => {
                return Err(vec![CatalogDefect::Discovery {
                    error: error.to_string(),
                }]);
            }
        };

        let mut defects = Vec::new();
        let mut warnings = Vec::new();

        if discovered.tools.is_empty() {
            defects.push(CatalogDefect::NoOfferableTools {
                rejected: 0,
                first_error: "empty discovery responses are not activatable catalogs".to_string(),
            });
        }

        let policy = check_connector_policy(ConnectorCandidateFacts {
            server,
            discovered_tools: discovered.tools.len(),
        });
        for violation in policy.defects {
            defects.push(CatalogDefect::Policy { violation });
        }
        warnings.extend(policy.warnings.iter().map(ToString::to_string));

        // Structural registration check on a scratch registry: model-unsafe and
        // duplicate qualified references are found here rather than after the
        // connector's previous tools have already been removed.
        let mut scratch = ToolRegistry::new();
        let mut rejections = Vec::new();
        for tool in &discovered.tools {
            if let Err(error) = scratch.register_mcp_tool(&server.name, tool.clone()) {
                rejections.push(error.to_string());
            }
        }
        if !rejections.is_empty() {
            if rejections.len() == discovered.tools.len() {
                defects.push(CatalogDefect::NoOfferableTools {
                    rejected: rejections.len(),
                    first_error: rejections
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "unknown registration failure".to_string()),
                });
            } else {
                warnings.extend(rejections);
            }
        }

        let connector_revision = connector_revision(server, &discovered.protocol_version);
        let schema_hash = candidate_schema_hash(&server.name, &scratch);
        if defects.is_empty() {
            Ok(StagedConnector {
                connector_revision,
                schema_hash,
                protocol_version: discovered.protocol_version,
                tools: discovered.tools,
                client: discovered.client,
                catalog_fresh_until: discovered.catalog_fresh_until,
                warnings,
            })
        } else {
            Err(defects)
        }
    }

    async fn discover_server_tools(&self, server: &McpServerConfig) -> Result<DiscoveredConnector> {
        let headers = self.mcp.headers_for(server)?;
        discover_server_tools(server, headers).await
    }
}

/// Counts the tools one connector currently contributes to a registry.
fn retained_tool_count(registry: &ToolRegistry, server_name: &str) -> usize {
    registry
        .default_loadout()
        .iter()
        .filter(|name| {
            matches!(
                registry.tools.get(*name).map(|tool| &tool.execution),
                Some(ToolExecution::Mcp { server_name: owner, .. }) if owner == server_name
            )
        })
        .count()
}

/// Which discovery pass is running.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DiscoveryPass {
    /// Router construction: lazily configured connectors are deferred.
    Startup,
    /// Background refresh: every connector is attempted.
    Refresh,
}

impl DiscoveryPass {
    fn defers(self, server: &McpServerConfig) -> bool {
        self == Self::Startup && !server.discovery.is_eager()
    }
}

/// One connector's discovery result.
struct DiscoveredConnector {
    protocol_version: String,
    tools: Vec<McpDiscoveredToolRegistration>,
    client: Arc<MCPClient>,
    catalog_fresh_until: Instant,
}

/// One candidate connector that cleared every deterministic check.
struct StagedConnector {
    connector_revision: String,
    schema_hash: String,
    protocol_version: String,
    tools: Vec<McpDiscoveredToolRegistration>,
    client: Arc<MCPClient>,
    catalog_fresh_until: Instant,
    warnings: Vec<String>,
}

/// Connects to one connector and lists its tools.
///
/// The client used for discovery is handed back rather than thrown away, so a
/// connector discovered here reuses the same transport configuration on its first
/// tool call. Connectors that are never discovered — lazy ones, and ones added
/// between refreshes — are connected by [`ToolRouter::mcp_client`] on first use,
/// so a configured connector nobody calls still holds no socket.
async fn discover_server_tools(
    server: &McpServerConfig,
    headers: std::collections::HashMap<String, String>,
) -> Result<DiscoveredConnector> {
    let client = Arc::new(MCPClient::connect(server, headers).await?);
    let mut tools = client.list_tools().await?;
    // A server is free to return `tools/list` in any order, and some return
    // insertion order that changes as tools are edited. Sorting here is what
    // makes "same inputs and revision yield the same schemas and order" a
    // property of the catalog rather than of the remote server's mood.
    tools.sort_by(|left, right| left.tool().name.cmp(&right.tool().name));
    tools.dedup_by(|left, right| left.tool().name == right.tool().name);
    let catalog_fresh_until = client.catalog_fresh_until().ok_or_else(|| {
        MoaError::StreamError("MCP server omitted catalog freshness metadata".to_string())
    })?;
    Ok(DiscoveredConnector {
        protocol_version: client.protocol_version().to_string(),
        tools,
        client,
        catalog_fresh_until,
    })
}

/// Computes the revision identifying one candidate connector.
///
/// Covers the connector's configured identity and its fixed protocol
/// revision but not its tool schemas, which the candidate's schema hash covers
/// separately. Keeping them apart is what lets a report say "same connector,
/// new schemas" instead of collapsing both into one opaque change.
fn connector_revision(server: &McpServerConfig, protocol_version: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"moa.mcp.connector-revision.v1");
    absorb(&mut hasher, server.name.as_bytes());
    absorb(&mut hasher, server.url.as_bytes());
    absorb(&mut hasher, protocol_version.as_bytes());
    hex::encode(hasher.finalize())
}

/// Computes the schema hash of one candidate connector's offerable tools.
fn candidate_schema_hash(server_name: &str, scratch: &ToolRegistry) -> String {
    let mut entries = scratch
        .capability_registrations()
        .into_iter()
        .filter_map(|(definition, execution)| match execution {
            ToolExecution::Mcp {
                server_name: owner,
                schema_hash,
                ..
            } if owner == server_name => Some((definition.name, schema_hash)),
            _ => None,
        })
        .collect::<Vec<_>>();
    entries.sort();

    let mut hasher = Sha256::new();
    hasher.update(b"moa.mcp.candidate-schema.v1");
    absorb(&mut hasher, server_name.as_bytes());
    for (name, schema_hash) in entries {
        absorb(&mut hasher, name.as_bytes());
        absorb(&mut hasher, schema_hash.as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// Computes the revision of the MCP portion of one catalog.
fn mcp_catalog_revision(registry: &ToolRegistry) -> String {
    let mut entries = registry
        .tools
        .iter()
        .filter_map(|(name, tool)| match &tool.execution {
            ToolExecution::Mcp {
                server_name,
                schema_hash,
                ..
            } => Some((name.clone(), server_name.clone(), schema_hash.clone())),
            ToolExecution::BuiltIn(_)
            | ToolExecution::Hand { .. }
            | ToolExecution::InstalledConnectorAction { .. } => None,
        })
        .collect::<Vec<_>>();
    entries.sort();

    let mut hasher = Sha256::new();
    hasher.update(b"moa.mcp.catalog-revision.v1");
    for (name, server_name, schema_hash) in entries {
        for part in [name, server_name, schema_hash] {
            absorb(&mut hasher, part.as_bytes());
        }
    }
    hex::encode(hasher.finalize())
}

/// Runs MCP catalog refresh on an interval for the life of the process.
///
/// Returns immediately with the spawned handle. Refresh failures are per
/// connector and already recorded as health, so the loop itself has nothing to
/// fail on and never needs to stop the deployment.
pub fn spawn_mcp_catalog_refresh(
    router: Arc<ToolRouter>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick completes immediately; skip it so the loop does not
        // re-discover everything the composition root just discovered.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = router.mcp.wait_for_refresh_request() => {}
            }
            let refresh = router.refresh_mcp_catalog().await;
            tracing::debug!(
                revision = %refresh.revision,
                contract_hash = %refresh.activation.pin.contract_hash,
                connectors = refresh.health.len(),
                quarantined = refresh.activation.quarantined.len(),
                degraded = refresh
                    .health
                    .values()
                    .filter(|health| !matches!(health, McpConnectorHealth::Ready { .. }))
                    .count(),
                "refreshed the MCP tool catalog"
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::adapters::mcp::McpDiscoveredTool;
    use crate::core::ToolRegistry;

    use super::{PinnedToolOwner, ToolCatalogPin};

    fn discovered(name: &str, schema: serde_json::Value) -> McpDiscoveredTool {
        McpDiscoveredTool {
            name: name.to_string(),
            description: "fixture".to_string(),
            input_schema: schema,
        }
    }

    fn pin_with(server: &str, tool: &str, schema: serde_json::Value) -> ToolCatalogPin {
        let mut registry = ToolRegistry::new();
        registry
            .register_mcp_tool(server, discovered(tool, schema))
            .expect("register fixture connector tool");
        ToolCatalogPin::from_registry(&registry).expect("pin fixture registry")
    }

    #[test]
    fn a_pin_covers_local_tools_as_well_as_connector_tools_offline() {
        // Pins: the snapshot is the whole catalog, not the connector slice. A
        // conversational or durable contract pin must also notice a deploy that
        // changed a built-in tool's contract.
        let pin = ToolCatalogPin::from_registry(&ToolRegistry::default_local())
            .expect("pin the default local registry");

        assert!(
            pin.tools
                .iter()
                .any(|tool| tool.owner == PinnedToolOwner::Hand && tool.tool == "bash"),
            "hand tools must be pinned: {:?}",
            pin.tools.iter().map(|tool| &tool.tool).collect::<Vec<_>>()
        );
        assert!(
            pin.tools
                .iter()
                .any(|tool| tool.owner == PinnedToolOwner::BuiltIn),
            "built-in tools must be pinned"
        );
        assert!(
            pin.tools
                .iter()
                .all(|tool| !tool.contract_revision.is_empty()),
            "every pinned tool needs a governed contract revision"
        );
    }

    #[test]
    fn changing_a_tool_schema_changes_its_governed_contract_revision_offline() {
        // Pins: a connector schema change invalidates the complete governed
        // contract revision carried by invocation snapshots.
        let before = pin_with(
            "api",
            "search",
            json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        );
        let after = pin_with(
            "api",
            "search",
            json!({"type": "object", "properties": {"query": {"type": "string"}}}),
        );

        assert_ne!(before.contract_hash, after.contract_hash);
        let reference = crate::core::mcp_tool_reference("api", "search");
        assert_ne!(
            before.contract_revision(&reference),
            after.contract_revision(&reference)
        );
    }

    #[test]
    fn policy_and_budget_changes_move_the_governed_contract_without_moving_schema_offline() {
        // Pins: durable execution is governed by more than model-visible JSON.
        // Changing an output budget (and, by the same canonical definition,
        // policy/retry/route metadata) must invalidate the catalog even when the
        // input schema is byte-identical.
        let before_registry = ToolRegistry::default_local();
        let before_schema = before_registry
            .tools
            .get("bash")
            .expect("bash registration")
            .definition
            .schema
            .clone();
        let before = ToolCatalogPin::from_registry(&before_registry).expect("pin before");
        let mut after_registry = before_registry;
        after_registry
            .tools
            .get_mut("bash")
            .expect("bash registration")
            .definition
            .max_output_tokens += 1;
        let after = ToolCatalogPin::from_registry(&after_registry).expect("pin after");

        assert_eq!(
            before_schema,
            after_registry
                .tools
                .get("bash")
                .expect("bash registration")
                .definition
                .schema
        );
        assert_ne!(
            before.contract_revision("bash"),
            after.contract_revision("bash")
        );
        assert_ne!(before.contract_hash, after.contract_hash);
    }
}
