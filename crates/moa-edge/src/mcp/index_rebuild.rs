//! Operator tools for storage-partition index rebuilds.
//!
//! Every tool here is tenant-scoped by the transport: the tenant comes from the
//! authenticated MCP session, never from tool input, and the orchestrator
//! handler behind each path re-checks tenant-admin authority before it reads or
//! writes rebuild state. An operator cannot name someone else's tenant.
//!
//! The destructive hints are deliberate. `index_rebuild_start` rewrites every
//! vector in a partition; `index_rebuild_rollback` moves what the whole tenant
//! retrieves back a generation; `index_rebuild_finalize` discards the retired
//! generation and ends the rollback window for good. Only `index_rebuild_status`
//! is read-only.

use moa_wire::memory::{
    RebuildActionResponse, RebuildOperationRequest, RebuildStartRequest, RebuildStatusResponse,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{RoleServer, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Server;
use super::command::ServicePath;

const REBUILD_START: ServicePath = ServicePath::new("/GraphMemoryMaint/start_index_rebuild");
const REBUILD_STATUS: ServicePath = ServicePath::new("/GraphMemoryMaint/index_rebuild_status");
const REBUILD_CANCEL: ServicePath = ServicePath::new("/GraphMemoryMaint/cancel_index_rebuild");
const REBUILD_ROLLBACK: ServicePath = ServicePath::new("/GraphMemoryMaint/rollback_index_rebuild");
const REBUILD_FINALIZE: ServicePath = ServicePath::new("/GraphMemoryMaint/finalize_index_rebuild");

/// Build the index-rebuild operator tool router.
pub(super) fn router() -> rmcp::handler::server::router::tool::ToolRouter<Server> {
    Server::index_rebuild_router()
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RebuildKindInput {
    /// Recompute every vector in the partition under the configured embedder.
    Reembed,
    /// Recompute chunk boundaries and everything derived from them.
    Rechunk,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct RebuildStartInput {
    /// Which rebuild to run against the authenticated tenant's partition.
    kind: RebuildKindInput,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct RebuildOperationInput {
    /// Rebuild operation UUID returned by `index_rebuild_start`.
    operation_uid: Uuid,
}

#[tool_router(router = index_rebuild_router, vis = "pub(super)")]
impl Server {
    /// Start a durable re-embed or rechunk of the tenant's storage partition.
    ///
    /// The partition keeps serving its current vectors throughout. The rebuild
    /// builds a candidate generation, validates it against the one it would
    /// replace, and only then activates. Ordinary memory writes are fenced for
    /// the duration.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn index_rebuild_start(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<RebuildStartInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, RebuildStartRequest, RebuildStatusResponse>(
            context,
            &input,
            REBUILD_START,
            "Started a storage-partition index rebuild.",
        )
        .await
    }

    /// Report a rebuild's generation ids, exact counts, cost estimate, rate and
    /// retry state, and last safe error.
    ///
    /// Cost is an estimate derived from input size. The embedding provider
    /// reports no billed usage, so no field here is a bill.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn index_rebuild_status(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<RebuildOperationInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, RebuildOperationRequest, RebuildStatusResponse>(
            context,
            &input,
            REBUILD_STATUS,
            "Reported index-rebuild status.",
        )
        .await
    }

    /// Ask a running rebuild to stop at its next committed checkpoint.
    ///
    /// Cooperative rather than immediate: the build finishes the batch it is
    /// on, so progress stays consistent with the recorded checkpoint.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn index_rebuild_cancel(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<RebuildOperationInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, RebuildOperationRequest, RebuildActionResponse>(
            context,
            &input,
            REBUILD_CANCEL,
            "Requested index-rebuild cancellation.",
        )
        .await
    }

    /// Restore the previous generation as the tenant's production read generation.
    ///
    /// Available only while the retired generation is retained — that is, after
    /// activation and before finalization.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn index_rebuild_rollback(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<RebuildOperationInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, RebuildOperationRequest, RebuildActionResponse>(
            context,
            &input,
            REBUILD_ROLLBACK,
            "Rolled the index rebuild back to the previous generation.",
        )
        .await
    }

    /// Discard the retired generation and close the rollback window.
    ///
    /// Irreversible: after this the previous generation's vectors are gone and
    /// no reader can reconstruct the retired contract.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn index_rebuild_finalize(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<RebuildOperationInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, RebuildOperationRequest, RebuildActionResponse>(
            context,
            &input,
            REBUILD_FINALIZE,
            "Finalized the index rebuild and discarded the retired generation.",
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_tool_inputs_cannot_name_another_tenant() {
        // Pins: the operator tool schema carries no tenant field. The tenant is
        // stamped from the authenticated MCP session, so a crafted tool call
        // cannot start or roll back a rebuild in a tenant the caller does not
        // administer.
        let start = serde_json::to_value(RebuildStartInput {
            kind: RebuildKindInput::Reembed,
        })
        .expect("start input encodes");
        let operation = serde_json::to_value(RebuildOperationInput {
            operation_uid: Uuid::nil(),
        })
        .expect("operation input encodes");

        for input in [&start, &operation] {
            let object = input.as_object().expect("tool input is an object");
            assert!(
                !object.contains_key("tenant_id"),
                "tool inputs must not accept a caller-supplied tenant: {input}"
            );
            assert!(!object.contains_key("storage_partition_id"));
        }
    }

    #[test]
    fn rebuild_tool_kinds_serialize_to_the_wire_vocabulary() {
        // Pins: the MCP enum encodes to the same discriminators the wire DTO
        // and the V000351 CHECK constraint use, so a valid tool call cannot be
        // rejected at the database boundary.
        assert_eq!(
            serde_json::to_value(RebuildKindInput::Reembed).expect("encodes"),
            serde_json::json!("reembed")
        );
        assert_eq!(
            serde_json::to_value(RebuildKindInput::Rechunk).expect("encodes"),
            serde_json::json!("rechunk")
        );
    }
}
