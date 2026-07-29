//! Artifact editing and human-reviewed learning-candidate MCP tools.

use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::experience::LearningCandidate;
use moa_wire::artifacts::{
    ArtifactExportRequest, ArtifactExportResponse, ArtifactFileDocument, ArtifactImportRequest,
    ArtifactImportResponse, ArtifactListRequest, ArtifactListResponse, ArtifactPublishRequest,
    ArtifactPublishResponse, ArtifactValidateRequest, ArtifactValidateResponse,
};
use moa_wire::session_store::{
    GetLearningCandidateRequest, LearningCandidateReviewAction, LearningCandidateReviewRequest,
    LearningCandidateReviewResponse,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{RoleServer, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Server;
use super::command::ServicePath;
use super::{request_identity_and_headers, result};
use crate::mcp::command::McpCommandClient;

const ARTIFACTS_LIST: ServicePath = ServicePath::new("/Artifacts/list");
const ARTIFACTS_EXPORT: ServicePath = ServicePath::new("/Artifacts/export");
const ARTIFACTS_VALIDATE: ServicePath = ServicePath::new("/Artifacts/validate");
const ARTIFACTS_IMPORT: ServicePath = ServicePath::new("/Artifacts/import");
const ARTIFACTS_PUBLISH: ServicePath = ServicePath::new("/Artifacts/publish");
const LEARNING_GET: ServicePath = ServicePath::new("/LearningReview/get");
const LEARNING_ACCEPT: ServicePath = ServicePath::new("/LearningReview/accept_skill");
const LEARNING_REJECT: ServicePath = ServicePath::new("/LearningReview/reject");
const LEARNING_ACCEPT_ROLLBACK: ServicePath = ServicePath::new("/LearningReview/accept_rollback");
const LEARNING_DISMISS: ServicePath = ServicePath::new("/LearningReview/dismiss");

/// Build the artifact and learning-review tool router.
pub(super) fn router() -> rmcp::handler::server::router::tool::ToolRouter<Server> {
    Server::artifacts_learning_router()
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ArtifactKindInput {
    Agent,
    Skill,
    Connector,
    Action,
    ExperimentPlan,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ArtifactStatusInput {
    Draft,
    Published,
    Archived,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ArtifactSourceFormatInput {
    Json,
    Yaml,
}

impl ArtifactSourceFormatInput {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Yaml => "yaml",
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ArtifactListInput {
    /// Optional artifact family to return.
    kind: Option<ArtifactKindInput>,
    /// Optional exact lifecycle status to return.
    status: Option<ArtifactStatusInput>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ArtifactExportInput {
    /// Artifact family containing the named artifact.
    kind: ArtifactKindInput,
    /// Stable artifact name.
    name: String,
    /// Optional returned source format; omit to use the artifact's stored format.
    source_format: Option<ArtifactSourceFormatInput>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ArtifactValidateInput {
    /// Syntax used by `source_text`.
    source_format: ArtifactSourceFormatInput,
    /// Complete JSON or YAML artifact document to validate; do not pass a filesystem path.
    source_text: String,
    /// Intended lifecycle status whose stricter validation rules should be applied.
    status: Option<ArtifactStatusInput>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ArtifactFileInput {
    /// POSIX relative path inside the artifact package.
    path: String,
    /// Base64-encoded file content.
    content_base64: String,
    /// Optional media type hint.
    content_type: Option<String>,
    /// Whether the file should be executable in a sandbox.
    #[serde(default)]
    executable: bool,
}

impl From<ArtifactFileInput> for ArtifactFileDocument {
    fn from(value: ArtifactFileInput) -> Self {
        Self {
            path: value.path,
            content_base64: value.content_base64,
            content_type: value.content_type,
            executable: value.executable,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ArtifactImportInput {
    /// Syntax used by `source_text`.
    source_format: ArtifactSourceFormatInput,
    /// Raw artifact source document. Import always creates a draft revision.
    source_text: String,
    /// Optional package files stored with the revision.
    #[serde(default)]
    files: Vec<ArtifactFileInput>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ArtifactPublishInput {
    /// Exact draft revision to publish.
    revision_uid: Uuid,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct LearningCandidateInput {
    /// Learning candidate identifier.
    candidate_id: Uuid,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct LearningReviewInput {
    /// Learning candidate identifier.
    candidate_id: Uuid,
    /// Optional human-readable review reason.
    reason: Option<String>,
}

#[tool_router(router = artifacts_learning_router)]
impl Server {
    /// List artifacts visible to the authenticated tenant.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn artifacts_list(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ArtifactListInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ArtifactListRequest, ArtifactListResponse>(
            context,
            &input,
            ARTIFACTS_LIST,
            "Listed tenant artifacts.",
        )
        .await
    }

    /// Export one visible artifact revision with its source and package files.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn artifact_export(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ArtifactExportInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ArtifactExportRequest, ArtifactExportResponse>(
            context,
            &input,
            ARTIFACTS_EXPORT,
            "Exported artifact revision.",
        )
        .await
    }

    /// Validate an artifact source document without writing it.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn artifact_validate(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ArtifactValidateInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ArtifactValidateRequest, ArtifactValidateResponse>(
            context,
            &input,
            ARTIFACTS_VALIDATE,
            "Validated artifact source.",
        )
        .await
    }

    /// Import artifact source as a new draft revision; this never publishes it.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn artifact_import(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ArtifactImportInput>,
    ) -> CallToolResult {
        let (identity, headers) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let request = ArtifactImportRequest {
            scope: ActionRuleScope::Tenant {
                tenant_id: identity.tenant_id,
            },
            source_format: input.source_format.as_str().to_owned(),
            source_text: input.source_text,
            files: input.files.into_iter().map(Into::into).collect(),
        };
        let command = McpCommandClient::new(self.state.proxy.as_ref(), &identity, &headers);
        result::command_result(
            "Imported draft artifact revision.",
            command
                .call::<_, ArtifactImportResponse>(ARTIFACTS_IMPORT, &request)
                .await,
        )
    }

    /// Publish an exact draft artifact revision, changing active tenant behavior.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn artifact_publish(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ArtifactPublishInput>,
    ) -> CallToolResult {
        let (identity, headers) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let request = ArtifactPublishRequest {
            scope: ActionRuleScope::Tenant {
                tenant_id: identity.tenant_id,
            },
            revision_uid: input.revision_uid,
        };
        let command = McpCommandClient::new(self.state.proxy.as_ref(), &identity, &headers);
        result::command_result(
            "Published artifact revision.",
            command
                .call::<_, ArtifactPublishResponse>(ARTIFACTS_PUBLISH, &request)
                .await,
        )
    }

    /// Load one full learning candidate for operator review.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn learning_candidate_get(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<LearningCandidateInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, GetLearningCandidateRequest, LearningCandidate>(
            context,
            &input,
            LEARNING_GET,
            "Loaded learning candidate.",
        )
        .await
    }

    /// Accept a proposed skill candidate through the existing regression and publish gate.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = true
    ))]
    async fn learning_candidate_accept_skill(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<LearningReviewInput>,
    ) -> CallToolResult {
        self.review_candidate(
            context,
            input,
            LearningCandidateReviewAction::Accept,
            LEARNING_ACCEPT,
        )
        .await
    }

    /// Accept a rollback proposal, archiving the regressed published revision.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = true
    ))]
    async fn learning_candidate_accept_rollback(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<LearningReviewInput>,
    ) -> CallToolResult {
        self.review_candidate(
            context,
            input,
            LearningCandidateReviewAction::Accept,
            LEARNING_ACCEPT_ROLLBACK,
        )
        .await
    }

    /// Dismiss an informational learning candidate that no code can apply.
    ///
    /// Not destructive: dismissal closes a review item and publishes nothing.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn learning_candidate_dismiss(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<LearningReviewInput>,
    ) -> CallToolResult {
        self.review_candidate(
            context,
            input,
            LearningCandidateReviewAction::Dismiss,
            LEARNING_DISMISS,
        )
        .await
    }

    /// Reject a proposed learning candidate while preserving its draft evidence.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn learning_candidate_reject(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<LearningReviewInput>,
    ) -> CallToolResult {
        self.review_candidate(
            context,
            input,
            LearningCandidateReviewAction::Reject,
            LEARNING_REJECT,
        )
        .await
    }
}

impl Server {
    async fn review_candidate(
        &self,
        context: RequestContext<RoleServer>,
        input: LearningReviewInput,
        action: LearningCandidateReviewAction,
        path: ServicePath,
    ) -> CallToolResult {
        let (identity, headers) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let request = LearningCandidateReviewRequest {
            tenant_id: identity.tenant_id,
            candidate_id: input.candidate_id,
            action,
            reviewer_subject: String::new(),
            reason: input.reason,
        };
        let command = McpCommandClient::new(self.state.proxy.as_ref(), &identity, &headers);
        result::command_result(
            "Recorded learning candidate review.",
            command
                .call::<_, LearningCandidateReviewResponse>(path, &request)
                .await,
        )
    }
}
