//! MCP connector health, deterministic catalog discovery, and background refresh.
//!
//! A connector's tools enter the router through one path only: a discovery pass
//! that produces a whole new catalog snapshot and publishes it atomically. That
//! shape is what lets a single connector fail without touching anyone else's
//! tools, lets a refresh keep serving a connector's last-known-good tools while
//! it is down, and guarantees no reader ever sees a half-applied refresh.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_config::{McpServerConfig, MoaConfig};
use moa_core::{error::MoaError, error::Result};
use sha2::{Digest, Sha256};

use crate::adapters::mcp::{MCPClient, McpDiscoveredToolRegistration};

use super::registration::ToolExecution;
use super::{ToolRegistry, ToolRouter};

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
        /// Protocol revision negotiated during the successful handshake.
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
        matches!(self, Self::Ready { .. } | Self::Degraded { .. })
    }

    /// Returns the stable machine-readable state label.
    #[must_use]
    pub fn state(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready { .. } => "ready",
            Self::Degraded { .. } => "degraded",
            Self::Unavailable { .. } => "unavailable",
        }
    }

    /// Returns the failure detail, when this connector's last attempt failed.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Degraded { error, .. } | Self::Unavailable { error, .. } => Some(error.as_str()),
            Self::Pending | Self::Ready { .. } => None,
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

/// Outcome of one catalog discovery pass.
#[derive(Clone, Debug)]
pub struct McpCatalogRefresh {
    /// Typed health for every configured connector after the pass.
    pub health: BTreeMap<String, McpConnectorHealth>,
    /// Catalog revision published by the pass.
    pub revision: String,
}

impl ToolRouter {
    /// Returns the typed health of every configured MCP connector.
    pub async fn mcp_connector_health(&self) -> BTreeMap<String, McpConnectorHealth> {
        self.mcp_health.read().await.clone()
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
        mcp_catalog_revision(&self.registry())
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
            .await;
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
    /// Safe to call on a serving router: the new catalog is assembled off to the
    /// side and swapped in as a whole, and a connector that fails keeps serving
    /// its last-known-good tools rather than losing them to a transient error.
    pub async fn refresh_mcp_catalog(&self) -> McpCatalogRefresh {
        let servers = self.configured_mcp_servers();
        self.run_mcp_discovery(&servers, DiscoveryPass::Refresh)
            .await
    }

    /// Returns the configured connectors in their authored order.
    fn configured_mcp_servers(&self) -> Vec<McpServerConfig> {
        let mut servers = self.mcp_servers.values().cloned().collect::<Vec<_>>();
        servers.sort_by(|left, right| left.name.cmp(&right.name));
        servers
    }

    async fn run_mcp_discovery(
        &self,
        servers: &[McpServerConfig],
        pass: DiscoveryPass,
    ) -> McpCatalogRefresh {
        let mut registry = (*self.registry()).clone();
        let mut health = self.mcp_health.read().await.clone();

        for server in servers {
            if pass.defers(server) {
                health
                    .entry(server.name.clone())
                    .or_insert(McpConnectorHealth::Pending);
                continue;
            }
            let observed_at = Utc::now();
            match discover_server_tools(server).await {
                Ok(discovered) => {
                    self.mcp_clients
                        .write()
                        .await
                        .insert(server.name.clone(), discovered.client);
                    registry.remove_mcp_server_tools(&server.name);
                    let mut registered = 0_usize;
                    for tool in discovered.tools {
                        match registry.register_mcp_tool(
                            &server.name,
                            server.credential_scope,
                            tool,
                        ) {
                            Ok(_) => registered += 1,
                            Err(error) => tracing::warn!(
                                mcp_server = %server.name,
                                %error,
                                "skipped an MCP tool that cannot be offered to a model"
                            ),
                        }
                    }
                    health.insert(
                        server.name.clone(),
                        McpConnectorHealth::Ready {
                            tools: registered,
                            protocol_version: discovered.protocol_version,
                            observed_at,
                        },
                    );
                }
                Err(error) => {
                    let retained = registry
                        .default_loadout()
                        .iter()
                        .filter(|name| {
                            matches!(
                                registry.tools.get(*name).map(|tool| &tool.execution),
                                Some(ToolExecution::Mcp { server_name, .. })
                                    if server_name == &server.name
                            )
                        })
                        .count();
                    let last_good_at = match health.get(&server.name) {
                        Some(McpConnectorHealth::Ready { observed_at, .. }) => Some(*observed_at),
                        Some(McpConnectorHealth::Degraded { last_good_at, .. }) => {
                            Some(*last_good_at)
                        }
                        _ => None,
                    };
                    let next = match last_good_at.filter(|_| retained > 0) {
                        Some(last_good_at) => McpConnectorHealth::Degraded {
                            tools: retained,
                            last_good_at,
                            error: error.to_string(),
                            observed_at,
                        },
                        None => {
                            registry.remove_mcp_server_tools(&server.name);
                            McpConnectorHealth::Unavailable {
                                error: error.to_string(),
                                observed_at,
                            }
                        }
                    };
                    tracing::warn!(
                        mcp_server = %server.name,
                        required = server.required,
                        health = next.state(),
                        "MCP connector discovery failed"
                    );
                    health.insert(server.name.clone(), next);
                }
            }
        }

        registry.apply_budgets(&self.tool_budgets);
        let revision = mcp_catalog_revision(&registry);
        self.publish_registry(registry);
        self.refresh_unmatched_permission_patterns();
        *self.mcp_health.write().await = health.clone();
        McpCatalogRefresh { health, revision }
    }
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
}

/// Connects to one connector and lists its tools.
///
/// The handshake this performs is handed back rather than thrown away, so a
/// connector discovered here does not pay a second handshake on its first tool
/// call. Connectors that are never discovered — lazy ones, and ones added
/// between refreshes — are connected by [`ToolRouter::mcp_client`] on first use,
/// so a configured connector nobody calls still holds no socket.
async fn discover_server_tools(server: &McpServerConfig) -> Result<DiscoveredConnector> {
    let client = Arc::new(MCPClient::connect(server).await?);
    let mut tools = client.list_tools().await?;
    // A server is free to return `tools/list` in any order, and some return
    // insertion order that changes as tools are edited. Sorting here is what
    // makes "same inputs and revision yield the same schemas and order" a
    // property of the catalog rather than of the remote server's mood.
    tools.sort_by(|left, right| left.tool().name.cmp(&right.tool().name));
    tools.dedup_by(|left, right| left.tool().name == right.tool().name);
    Ok(DiscoveredConnector {
        protocol_version: client.negotiated_protocol_version().to_string(),
        tools,
        client,
    })
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
            ToolExecution::BuiltIn(_) | ToolExecution::Hand { .. } => None,
        })
        .collect::<Vec<_>>();
    entries.sort();

    let mut hasher = Sha256::new();
    hasher.update(b"moa.mcp.catalog-revision.v1");
    for (name, server_name, schema_hash) in entries {
        for part in [name, server_name, schema_hash] {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part.as_bytes());
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
            ticker.tick().await;
            let refresh = router.refresh_mcp_catalog().await;
            tracing::debug!(
                revision = %refresh.revision,
                connectors = refresh.health.len(),
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
