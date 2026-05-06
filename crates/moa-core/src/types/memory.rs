//! Graph memory scope and skill metadata types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{UserId, WorkspaceId};

/// Three-tier memory scope walked from global to workspace to user during retrieval.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MemoryScope {
    /// Cross-workspace knowledge read by every workspace.
    Global,
    /// Workspace-tenant knowledge.
    Workspace {
        /// Workspace owning this memory scope.
        workspace_id: WorkspaceId,
    },
    /// User-personal knowledge inside a workspace.
    User {
        /// Workspace containing this user scope.
        workspace_id: WorkspaceId,
        /// User owning this memory scope.
        user_id: UserId,
    },
}

/// Fast discriminator for the three memory scope tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeTier {
    /// Cross-workspace global memory tier.
    Global,
    /// Workspace memory tier.
    Workspace,
    /// User memory tier within a workspace.
    User,
}

impl MemoryScope {
    /// Returns the ancestor chain from `Global` through this scope.
    pub fn ancestors(&self) -> Vec<MemoryScope> {
        match self {
            MemoryScope::Global => vec![MemoryScope::Global],
            MemoryScope::Workspace { workspace_id } => vec![
                MemoryScope::Global,
                MemoryScope::Workspace {
                    workspace_id: workspace_id.clone(),
                },
            ],
            MemoryScope::User {
                workspace_id,
                user_id,
            } => vec![
                MemoryScope::Global,
                MemoryScope::Workspace {
                    workspace_id: workspace_id.clone(),
                },
                MemoryScope::User {
                    workspace_id: workspace_id.clone(),
                    user_id: user_id.clone(),
                },
            ],
        }
    }

    /// Returns the workspace identifier for workspace and user scopes.
    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        match self {
            MemoryScope::Global => None,
            MemoryScope::Workspace { workspace_id } | MemoryScope::User { workspace_id, .. } => {
                Some(workspace_id.clone())
            }
        }
    }

    /// Returns the user identifier for user scopes.
    pub fn user_id(&self) -> Option<UserId> {
        match self {
            MemoryScope::User { user_id, .. } => Some(user_id.clone()),
            MemoryScope::Global | MemoryScope::Workspace { .. } => None,
        }
    }

    /// Returns whether this scope is the global tier.
    pub fn is_global(&self) -> bool {
        matches!(self, MemoryScope::Global)
    }

    /// Returns the tier discriminator for this memory scope.
    pub fn tier(&self) -> ScopeTier {
        match self {
            MemoryScope::Global => ScopeTier::Global,
            MemoryScope::Workspace { .. } => ScopeTier::Workspace,
            MemoryScope::User { .. } => ScopeTier::User,
        }
    }
}

/// Request-local scope values used to install Postgres RLS GUCs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScopeContext {
    scope: MemoryScope,
}

impl ScopeContext {
    /// Creates a scope context from a concrete memory scope.
    pub fn new(scope: MemoryScope) -> Self {
        Self { scope }
    }

    /// Creates a workspace-tier scope context.
    pub fn workspace(workspace_id: WorkspaceId) -> Self {
        Self::new(MemoryScope::Workspace { workspace_id })
    }

    /// Creates a user-tier scope context.
    pub fn user(workspace_id: WorkspaceId, user_id: UserId) -> Self {
        Self::new(MemoryScope::User {
            workspace_id,
            user_id,
        })
    }

    /// Returns the concrete memory scope for this context.
    pub fn scope(&self) -> &MemoryScope {
        &self.scope
    }

    /// Returns the workspace identifier for workspace and user scopes.
    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        self.scope.workspace_id()
    }

    /// Returns the user identifier for user scopes.
    pub fn user_id(&self) -> Option<UserId> {
        self.scope.user_id()
    }

    /// Returns the tier discriminator for this context.
    pub fn tier(&self) -> ScopeTier {
        self.scope.tier()
    }

    /// Returns the canonical SQL value for the scope tier.
    pub fn tier_str(&self) -> &'static str {
        match self.scope.tier() {
            ScopeTier::Global => "global",
            ScopeTier::Workspace => "workspace",
            ScopeTier::User => "user",
        }
    }
}

impl From<MemoryScope> for ScopeContext {
    fn from(scope: MemoryScope) -> Self {
        Self::new(scope)
    }
}

/// Tier-1 skill metadata injected into the context pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillMetadata {
    /// Canonical skill document path.
    pub path: String,
    /// Stable skill name from `SKILL.md`.
    pub name: String,
    /// Longer description from the Agent Skills frontmatter.
    pub description: String,
    /// User-defined tags.
    pub tags: Vec<String>,
    /// Tools referenced by the skill.
    pub allowed_tools: Vec<String>,
    /// Estimated token cost for the full skill body.
    pub estimated_tokens: usize,
    /// Historical usage count.
    pub use_count: u32,
    /// Last time the skill was used, when tracked in metadata.
    pub last_used: Option<DateTime<Utc>>,
    /// Historical success rate between `0.0` and `1.0`.
    pub success_rate: f32,
    /// Whether the skill was auto-generated.
    pub auto_generated: bool,
}
