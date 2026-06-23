//! Restate service for agent-facing contact identity operations.

use chrono::{DateTime, Duration, Utc};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::Identity;
use moa_core::{
    AgentSessionSelection, Channel, ChannelAccountId, ChannelAccountRef, ChannelRef, ContactId,
    ContactPointId, ContactPointInput, ContactPointKind, ContactPointRef, ContactRef,
    ContactSessionChannelChangeRequest, ContactSessionChannelChangeResponse,
    ContactSessionInitRequest, ContactSessionInitResponse, ContactSessionPromotionRequest,
    ContactSessionPromotionResponse, ContactTokenClaims, ContactTokenIssueRequest,
    ContactTokenIssueResponse, ContactVerificationChallengeId, ContactVerificationCompleteRequest,
    ContactVerificationCompleteResponse, ContactVerificationStartRequest,
    ContactVerificationStartResponse, ContactVerificationState, Event, ModelId, SessionActorRef,
    SessionMeta, SessionStatus, TenantId, WorkspaceId,
};
use moa_core::{MoaError, SessionStore};
use moa_messaging::{DeliveryMessage, DeliverySink, ProviderDeliverySink};
use moa_session::store::SessionChannelBindingReplacement;
use rand::Rng;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::objects::session::SessionClient;
use crate::services::session_store::inner::resolve_agent_context_for_session;

const LOW_ASSURANCE_SCOPES: &[&str] = &[
    "agent:session:create",
    "contact:session:channel:update",
    "contact:verify:start",
    "contact:verify:complete",
    "memory:session:read",
    "memory:session:write",
];
const VERIFIED_SCOPES: &[&str] = &[
    "agent:session:create",
    "contact:session:channel:update",
    "contact:verify:start",
    "contact:verify:complete",
    "contact:self:update",
    "contact:session:promote",
    "memory:session:read",
    "memory:session:write",
    "memory:self:read",
    "memory:self:write",
];
const MAX_VERIFICATION_ATTEMPTS: i32 = 5;

/// Restate surface for contact identity and contact-scoped sessions.
#[restate_sdk::service]
#[name = "Contacts"]
pub trait Contacts {
    /// Issues a low-assurance contact token for a tenant contact.
    async fn issue_token(
        request: Json<ContactTokenIssueRequest>,
    ) -> Result<Json<ContactTokenIssueResponse>, HandlerError>;

    /// Starts verification for a contact point owned by the current contact token.
    async fn start_verification(
        request: Json<ContactVerificationStartRequest>,
    ) -> Result<Json<ContactVerificationStartResponse>, HandlerError>;

    /// Completes contact-point verification and returns an upgraded token.
    async fn complete_verification(
        request: Json<ContactVerificationCompleteRequest>,
    ) -> Result<Json<ContactVerificationCompleteResponse>, HandlerError>;

    /// Creates a session initialized with contact metadata.
    async fn init_session(
        request: Json<ContactSessionInitRequest>,
    ) -> Result<Json<ContactSessionInitResponse>, HandlerError>;

    /// Changes the active communication channel for a contact-owned session.
    async fn change_session_channel(
        request: Json<ContactSessionChannelChangeRequest>,
    ) -> Result<Json<ContactSessionChannelChangeResponse>, HandlerError>;

    /// Promotes an existing session to the verified canonical contact.
    async fn promote_session(
        request: Json<ContactSessionPromotionRequest>,
    ) -> Result<Json<ContactSessionPromotionResponse>, HandlerError>;
}

/// Concrete contact service implementation.
#[derive(Clone, Default)]
pub struct ContactsImpl;

impl Contacts for ContactsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn issue_token(
        &self,
        ctx: Context<'_>,
        request: Json<ContactTokenIssueRequest>,
    ) -> Result<Json<ContactTokenIssueResponse>, HandlerError> {
        annotate_restate_handler_span("Contacts", "issue_token");
        let request = request.into_inner();
        let identity = require_identity(&ctx)?;
        authorize_tenant_operator(&identity, request.tenant_id).await?;
        let tenant_id = request.tenant_id;
        let token_issuer = contact_token_issuer()?;
        let pool = OrchestratorCtx::current_graph_pool();
        let requested_scopes = request.requested_scopes.clone();

        let (contact, contact_points) = ctx
            .run(|| async move {
                issue_contact(pool, tenant_id, request)
                    .await
                    .map(Json::from)
            })
            .name("contacts_issue_contact")
            .await?
            .into_inner();
        let contact = contact.with_scopes(low_assurance_scopes(&requested_scopes));
        annotate_contact_operation_span(&contact, None);
        let issued = token_issuer
            .issue_with_claims(&contact)
            .map_err(contact_token_handler_error)?;
        let grant_claims = issued.claims.clone();
        let grant_contact_id = contact.contact_id;
        let grant_expires_at = issued.expires_at;
        let issued_by_actor_id = identity.id;
        let grant_pool = OrchestratorCtx::current_graph_pool();
        ctx.run(|| async move {
            create_contact_token_grant(
                grant_pool,
                &grant_claims,
                grant_contact_id,
                grant_expires_at,
                "identity",
                Some(issued_by_actor_id),
            )
            .await
        })
        .name("contacts_record_issued_token_grant")
        .await?;
        let scopes = contact.scopes.clone();
        let permissions = contact.permissions.clone();

        Ok(Json::from(ContactTokenIssueResponse {
            contact,
            contact_points,
            token: issued.token,
            expires_at: issued.expires_at,
            scopes,
            permissions,
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: contact JWT verification and scope checks bound this operation to one contact and tenant.
    async fn start_verification(
        &self,
        ctx: Context<'_>,
        request: Json<ContactVerificationStartRequest>,
    ) -> Result<Json<ContactVerificationStartResponse>, HandlerError> {
        annotate_restate_handler_span("Contacts", "start_verification");
        let request = request.into_inner();
        let claims = verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "contact:verify:start")?;
        require_contact_session_permission(&claims, request.session_id)?;
        let contact_id = contact_id_from_claims(&claims)?;
        annotate_claim_contact_span(&claims, request.session_id);
        let tenant_id = claims.tenant_id;
        let pool = OrchestratorCtx::current_graph_pool();
        let store = OrchestratorCtx::current_session_store();
        let ttl_seconds = OrchestratorCtx::current_config()
            .auth
            .contact_tokens
            .verification_ttl_seconds;
        let messaging_config = OrchestratorCtx::current_config().messaging.clone();
        let delivery_channel = request.delivery_channel;
        let contact_point = request.contact_point;
        let session_id = request.session_id;

        Ok(ctx
            .run(|| async move {
                ensure_contact_token_grant_active(&pool, &claims, contact_id).await?;
                if let Some(session_id) = session_id {
                    validate_contact_session(
                        store.as_ref(),
                        session_id,
                        request.tenant_id,
                        contact_id,
                    )
                    .await?;
                }
                start_contact_verification(
                    pool,
                    ContactVerificationStartCommand {
                        tenant_id,
                        contact_id,
                        contact_point,
                        requested_channel: delivery_channel,
                        ttl_seconds,
                        messaging_config,
                    },
                )
                .await
                .map(Json::from)
            })
            .name("contacts_start_verification")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: contact JWT verification and one-time challenge verification bind promotion to the tenant contact point.
    async fn complete_verification(
        &self,
        ctx: Context<'_>,
        request: Json<ContactVerificationCompleteRequest>,
    ) -> Result<Json<ContactVerificationCompleteResponse>, HandlerError> {
        annotate_restate_handler_span("Contacts", "complete_verification");
        let request = request.into_inner();
        let claims = verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "contact:verify:complete")?;
        require_contact_session_permission(&claims, request.session_id)?;
        let contact_id = contact_id_from_claims(&claims)?;
        annotate_claim_contact_span(&claims, request.session_id);
        let tenant_id = claims.tenant_id;
        let token_issuer = contact_token_issuer()?;
        let pool = OrchestratorCtx::current_graph_pool();
        let store = OrchestratorCtx::current_session_store();
        let session_id = request.session_id;

        let contact = ctx
            .run(|| async move {
                ensure_contact_token_grant_active(&pool, &claims, contact_id).await?;
                if let Some(session_id) = session_id {
                    validate_contact_session(
                        store.as_ref(),
                        session_id,
                        request.tenant_id,
                        contact_id,
                    )
                    .await?;
                }
                complete_contact_verification(
                    pool,
                    tenant_id,
                    contact_id,
                    request.challenge_id,
                    request.code,
                )
                .await
                .map(Json::from)
            })
            .name("contacts_complete_verification")
            .await?
            .into_inner();
        let contact = contact.with_scopes(verified_scopes());
        let issued = token_issuer
            .issue_with_claims(&contact)
            .map_err(contact_token_handler_error)?;
        let grant_claims = issued.claims.clone();
        let grant_contact_id = contact.contact_id;
        let grant_expires_at = issued.expires_at;
        let grant_pool = OrchestratorCtx::current_graph_pool();
        ctx.run(|| async move {
            create_contact_token_grant(
                grant_pool,
                &grant_claims,
                grant_contact_id,
                grant_expires_at,
                "contact",
                Some(contact_id.0),
            )
            .await
        })
        .name("contacts_record_verified_token_grant")
        .await?;

        Ok(Json::from(ContactVerificationCompleteResponse {
            contact,
            token: issued.token,
            expires_at: issued.expires_at,
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: contact JWT verification and scope checks bound this session creation to one contact and tenant.
    async fn init_session(
        &self,
        ctx: Context<'_>,
        request: Json<ContactSessionInitRequest>,
    ) -> Result<Json<ContactSessionInitResponse>, HandlerError> {
        annotate_restate_handler_span("Contacts", "init_session");
        let request = request.into_inner();
        let claims = verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "agent:session:create")?;
        require_contact_agent_permission(&claims, &request.agent)?;
        let contact_id = contact_id_from_claims(&claims)?;
        annotate_claim_contact_span(&claims, None);
        let tenant_id = claims.tenant_id;
        let pool = OrchestratorCtx::current_graph_pool();
        let store = OrchestratorCtx::current_session_store();
        let storage_workspace_id = storage_workspace_id_for_tenant(tenant_id);
        let model = ModelId::new(request.model);
        let channel_request = request.channel;
        let initial_channel_ref = channel_request.channel_ref;
        let channel_reason = channel_request.reason;
        let agent_selection = request.agent.clone();
        let token_scopes = claims.scopes.clone();
        let token_permissions = claims.permissions.clone();
        let token_agent_ids = claims.agent_ids.clone();
        let token_session_ids = claims.session_ids.clone();
        let token_verified_contact_point_ids = claims.verified_contact_point_ids.clone();
        let token_linked_contact_ids = claims.linked_contact_ids.clone();

        let SessionChannelPreparation {
            contact,
            channel_ref,
            channel_account,
            contact_point_id,
        } = ctx
            .run(|| async move {
                ensure_contact_token_grant_active(&pool, &claims, contact_id).await?;
                let mut contact = load_contact_ref(pool.clone(), tenant_id, contact_id).await?;
                contact.scopes = token_scopes;
                contact.permissions = token_permissions;
                contact.agent_ids = token_agent_ids;
                contact.session_ids = token_session_ids;
                contact.verified_contact_point_ids = token_verified_contact_point_ids;
                contact.linked_contact_ids = token_linked_contact_ids;
                let resolved =
                    resolve_contact_session_channel(&pool, &contact, initial_channel_ref).await?;
                Ok::<_, HandlerError>(Json::from(SessionChannelPreparation {
                    contact,
                    channel_ref: resolved.channel_ref,
                    channel_account: resolved.channel_account,
                    contact_point_id: resolved.contact_point_id,
                }))
            })
            .name("contacts_prepare_session_channel")
            .await?
            .into_inner();
        let meta = SessionMeta {
            tenant_id,
            title: request.title,
            status: SessionStatus::Created,
            channel: channel_ref.channel(),
            active_channel_binding_id: None,
            model: model.clone(),
            contact: Some(contact.clone()),
            created_by: Some(SessionActorRef::Contact {
                id: contact.contact_id,
            }),
            ..SessionMeta::default()
        };
        let response_contact = contact.clone();
        let event_channel = meta.channel;
        let storage_workspace_id_for_create = storage_workspace_id.clone();
        let (session_id, meta_for_vo) = ctx
            .run(|| async move {
                let agent_context =
                    resolve_agent_context_for_session(store.as_ref(), &meta, &agent_selection)
                        .await?;
                let mut meta = meta;
                meta.agent_context = Some(agent_context);
                let session_id = store
                    .create_session(meta)
                    .await
                    .map_err(session_store_handler_error)?;
                store
                    .replace_session_channel_binding(SessionChannelBindingReplacement {
                        tenant_id,
                        workspace_id: &storage_workspace_id_for_create,
                        session_id,
                        contact_id: contact.contact_id,
                        channel_account_id: channel_account
                            .as_ref()
                            .map(|account| account.channel_account_id),
                        contact_point_id,
                        channel_ref: &channel_ref,
                        reason: channel_reason.as_deref(),
                    })
                    .await
                    .map_err(session_store_handler_error)?;
                store
                    .emit_event(
                        session_id,
                        Event::SessionCreated {
                            tenant_id,
                            contact_id: Some(contact.contact_id),
                            created_by: Some(SessionActorRef::Contact {
                                id: contact.contact_id,
                            }),
                            model,
                            channel: event_channel,
                        },
                    )
                    .await
                    .map_err(session_store_handler_error)?;
                let meta_for_vo = store
                    .get_session(session_id)
                    .await
                    .map_err(session_store_handler_error)?;
                Ok::<_, HandlerError>(Json::from((session_id, meta_for_vo)))
            })
            .name("contacts_create_session")
            .await?
            .into_inner();
        ctx.object_client::<SessionClient>(session_id.to_string())
            .set_meta(Json::from(meta_for_vo))
            .call()
            .await?;
        annotate_contact_operation_span(&response_contact, Some(session_id));

        Ok(Json::from(ContactSessionInitResponse {
            session_id,
            contact: response_contact,
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: contact JWT verification, scope checks, and session ownership validation bind this mutation to one contact session.
    async fn change_session_channel(
        &self,
        ctx: Context<'_>,
        request: Json<ContactSessionChannelChangeRequest>,
    ) -> Result<Json<ContactSessionChannelChangeResponse>, HandlerError> {
        annotate_restate_handler_span("Contacts", "change_session_channel");
        let request = request.into_inner();
        let claims = verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "contact:session:channel:update")?;
        require_contact_session_permission(&claims, Some(request.session_id))?;
        let contact_id = contact_id_from_claims(&claims)?;
        annotate_claim_contact_span(&claims, Some(request.session_id));
        let session_id = request.session_id;
        let tenant_id = claims.tenant_id;
        let pool = OrchestratorCtx::current_graph_pool();
        let store = OrchestratorCtx::current_session_store();
        let storage_workspace_id = storage_workspace_id_for_tenant(tenant_id);

        let ChannelChangeResult {
            contact,
            channel_ref,
            channel_account,
            meta,
        } = ctx
            .run(|| async move {
                ensure_contact_token_grant_active(&pool, &claims, contact_id).await?;
                let existing_meta = validate_contact_session(
                    store.as_ref(),
                    session_id,
                    request.tenant_id,
                    contact_id,
                )
                .await?;
                let contact = load_contact_ref(pool.clone(), tenant_id, contact_id).await?;
                let resolved =
                    resolve_contact_session_channel(&pool, &contact, request.channel_ref).await?;
                let binding_id = store
                    .replace_session_channel_binding(SessionChannelBindingReplacement {
                        tenant_id,
                        workspace_id: &storage_workspace_id,
                        session_id,
                        contact_id: contact.contact_id,
                        channel_account_id: resolved
                            .channel_account
                            .as_ref()
                            .map(|account| account.channel_account_id),
                        contact_point_id: resolved.contact_point_id,
                        channel_ref: &resolved.channel_ref,
                        reason: request.reason.as_deref(),
                    })
                    .await
                    .map_err(session_store_handler_error)?;
                store
                    .emit_event(
                        session_id,
                        Event::SessionChannelChanged {
                            from: existing_meta.channel,
                            to: resolved.channel_ref.channel(),
                            contact_id: Some(contact.contact_id),
                            from_binding_id: existing_meta.active_channel_binding_id,
                            to_binding_id: Some(binding_id),
                            changed_by: Some(SessionActorRef::Contact {
                                id: contact.contact_id,
                            }),
                            reason: request.reason,
                        },
                    )
                    .await
                    .map_err(session_store_handler_error)?;
                let meta = store
                    .get_session(session_id)
                    .await
                    .map_err(session_store_handler_error)?;
                Ok::<_, HandlerError>(Json::from(ChannelChangeResult {
                    contact,
                    channel_ref: resolved.channel_ref,
                    channel_account: resolved.channel_account,
                    meta,
                }))
            })
            .name("contacts_change_session_channel")
            .await?
            .into_inner();
        ctx.object_client::<SessionClient>(session_id.to_string())
            .set_meta(Json::from(meta))
            .call()
            .await?;
        annotate_contact_operation_span(&contact, Some(session_id));

        Ok(Json::from(ContactSessionChannelChangeResponse {
            session_id,
            contact,
            channel_ref,
            channel_account,
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: contact JWT verification and scope checks bind session promotion to the verified canonical contact.
    async fn promote_session(
        &self,
        ctx: Context<'_>,
        request: Json<ContactSessionPromotionRequest>,
    ) -> Result<Json<ContactSessionPromotionResponse>, HandlerError> {
        annotate_restate_handler_span("Contacts", "promote_session");
        let request = request.into_inner();
        let claims = verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "contact:session:promote")?;
        require_contact_session_permission(&claims, Some(request.session_id))?;
        if !claims.state.is_verified() {
            return Err(
                TerminalError::new_with_code(403, "verified contact token required").into(),
            );
        }
        let contact_id = contact_id_from_claims(&claims)?;
        annotate_claim_contact_span(&claims, Some(request.session_id));
        let tenant_id = claims.tenant_id;
        let pool = OrchestratorCtx::current_graph_pool();
        let store = OrchestratorCtx::current_session_store();

        let SessionPromotionResult {
            contact,
            promoted_from,
            mut meta,
        } = ctx
            .run(|| async move {
                ensure_contact_token_grant_active(&pool, &claims, contact_id).await?;
                let contact = load_contact_ref(pool.clone(), tenant_id, contact_id).await?;
                let meta = store
                    .get_session(request.session_id)
                    .await
                    .map_err(session_store_handler_error)?;
                if meta.tenant_id != request.tenant_id {
                    return Err(TerminalError::new_with_code(403, "session tenant mismatch").into());
                }
                let promoted_from =
                    promoted_from_contact(&pool, &meta, &contact, tenant_id).await?;
                store
                    .update_session_contact(request.session_id, contact.clone(), promoted_from)
                    .await
                    .map_err(session_store_handler_error)?;
                Ok::<_, HandlerError>(Json::from(SessionPromotionResult {
                    contact,
                    promoted_from,
                    meta,
                }))
            })
            .name("contacts_promote_session")
            .await?
            .into_inner();
        meta.contact = Some(contact.clone());
        meta.contact_promoted_from_id = promoted_from;
        ctx.object_client::<SessionClient>(request.session_id.to_string())
            .set_meta(Json::from(meta))
            .call()
            .await?;
        annotate_contact_operation_span(&contact, Some(request.session_id));

        Ok(Json::from(ContactSessionPromotionResponse {
            session_id: request.session_id,
            contact,
        }))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionPromotionResult {
    contact: ContactRef,
    promoted_from: Option<ContactId>,
    meta: SessionMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionChannelPreparation {
    contact: ContactRef,
    channel_ref: ChannelRef,
    channel_account: Option<ChannelAccountRef>,
    contact_point_id: Option<ContactPointId>,
}

#[derive(Debug, Clone)]
struct ResolvedSessionChannel {
    channel_ref: ChannelRef,
    channel_account: Option<ChannelAccountRef>,
    contact_point_id: Option<ContactPointId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChannelChangeResult {
    contact: ContactRef,
    channel_ref: ChannelRef,
    channel_account: Option<ChannelAccountRef>,
    meta: SessionMeta,
}

trait ContactScopesExt {
    fn with_scopes(self, scopes: Vec<String>) -> Self;
}

impl ContactScopesExt for ContactRef {
    fn with_scopes(mut self, scopes: Vec<String>) -> Self {
        self.scopes = scopes;
        self
    }
}

fn storage_workspace_id_for_tenant(tenant_id: TenantId) -> WorkspaceId {
    WorkspaceId::new(tenant_id.to_string())
}

async fn issue_contact(
    pool: sqlx::PgPool,
    tenant_id: TenantId,
    request: ContactTokenIssueRequest,
) -> Result<(ContactRef, Vec<ContactPointRef>), HandlerError> {
    let contact_id = ContactId::new();
    let state = if request.contact_points.is_empty() {
        ContactVerificationState::Anonymous
    } else {
        ContactVerificationState::Unverified
    };
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| db_handler_error("begin contact issuance", error))?;
    sqlx::query(
        r#"
        INSERT INTO contacts (
            id, tenant_id, workspace_id, contact_id, state, display_name, profile, metadata
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .bind(storage_workspace_id_for_tenant(tenant_id).as_str())
    .bind(contact_id.0)
    .bind(state.as_str())
    .bind(request.display_name.as_deref())
    .bind(&request.profile)
    .bind(&request.metadata)
    .execute(&mut *transaction)
    .await
    .map_err(|error| db_handler_error("insert contact", error))?;

    let mut contact_points = Vec::with_capacity(request.contact_points.len());
    for point in request.contact_points {
        let contact_point =
            insert_contact_point(&mut transaction, tenant_id, contact_id, point, false).await?;
        contact_points.push(contact_point);
    }

    transaction
        .commit()
        .await
        .map_err(|error| db_handler_error("commit contact issuance", error))?;

    Ok((
        ContactRef {
            contact_id,
            tenant_id,
            state,
            canonical_contact_id: None,
            linked_contact_ids: Vec::new(),
            scopes: Vec::new(),
            permissions: request.permissions,
            agent_ids: request.agent_ids,
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        },
        contact_points,
    ))
}

#[derive(Debug, Clone)]
struct ContactVerificationStartCommand {
    tenant_id: TenantId,
    contact_id: ContactId,
    contact_point: ContactPointInput,
    requested_channel: Option<Channel>,
    ttl_seconds: i64,
    messaging_config: moa_core::MessagingConfig,
}

async fn start_contact_verification(
    pool: sqlx::PgPool,
    command: ContactVerificationStartCommand,
) -> Result<ContactVerificationStartResponse, HandlerError> {
    let delivery = contact_point_delivery(&command.contact_point, command.requested_channel)?;
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| db_handler_error("begin contact verification", error))?;
    ensure_contact_in_tenant(&mut transaction, command.tenant_id, command.contact_id).await?;
    let contact_point = insert_contact_point(
        &mut transaction,
        command.tenant_id,
        command.contact_id,
        command.contact_point,
        false,
    )
    .await?;
    let challenge_id = ContactVerificationChallengeId::new();
    let code = verification_code();
    let expires_at = Utc::now() + Duration::seconds(command.ttl_seconds);
    sqlx::query(
        r#"
        UPDATE contact_verification_challenges
        SET consumed_at = NOW()
        WHERE contact_id = $1
          AND contact_point_id = $2
          AND tenant_id = $3
          AND consumed_at IS NULL
        "#,
    )
    .bind(command.contact_id.0)
    .bind(contact_point.id.0)
    .bind(command.tenant_id.0)
    .execute(&mut *transaction)
    .await
    .map_err(|error| db_handler_error("close previous contact verification challenges", error))?;
    sqlx::query(
        r#"
        INSERT INTO contact_verification_challenges
            (id, contact_id, contact_point_id, tenant_id, workspace_id, code_hash, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(challenge_id.0)
    .bind(command.contact_id.0)
    .bind(contact_point.id.0)
    .bind(command.tenant_id.0)
    .bind(storage_workspace_id_for_tenant(command.tenant_id).as_str())
    .bind(hash_verification_code(challenge_id, &code))
    .bind(expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(|error| db_handler_error("insert contact verification challenge", error))?;
    transaction
        .commit()
        .await
        .map_err(|error| db_handler_error("commit contact verification", error))?;
    let sink = match ProviderDeliverySink::from_env(
        storage_workspace_id_for_tenant(command.tenant_id).as_str(),
        &command.messaging_config,
    )
    .await
    {
        Ok(sink) => sink,
        Err(error) => {
            if let Err(consume_error) =
                consume_contact_verification_challenge(&pool, challenge_id).await
            {
                tracing::warn!(
                    challenge_id = %challenge_id,
                    error = %consume_error,
                    "failed to consume undelivered contact verification challenge"
                );
            }
            return Err(contact_delivery_handler_error(error));
        }
    };
    let delivery_message = DeliveryMessage::contact_verification_otp(
        command.tenant_id.0,
        storage_workspace_id_for_tenant(command.tenant_id),
        command.contact_id,
        delivery.channel,
        delivery.destination,
        &code,
        expires_at,
    );
    match sink.deliver(delivery_message).await {
        Ok(receipt) => {
            tracing::info!(
                challenge_id = %challenge_id,
                contact_id = %command.contact_id,
                contact_point_id = %contact_point.id,
                delivery_channel = receipt.channel.as_str(),
                provider = %receipt.provider,
                provider_message_id = ?receipt.provider_message_id,
                provider_status = ?receipt.provider_status,
                "contact verification challenge delivered"
            );
        }
        Err(error) => {
            if let Err(consume_error) =
                consume_contact_verification_challenge(&pool, challenge_id).await
            {
                tracing::warn!(
                    challenge_id = %challenge_id,
                    error = %consume_error,
                    "failed to consume undelivered contact verification challenge"
                );
            }
            return Err(contact_delivery_handler_error(error));
        }
    }
    tracing::info!(
        challenge_id = %challenge_id,
        contact_id = %command.contact_id,
        contact_point_id = %contact_point.id,
        delivery_channel = delivery.channel.as_str(),
        "contact verification challenge created"
    );
    Ok(ContactVerificationStartResponse {
        challenge_id,
        contact_point,
        delivery_channel: delivery.channel,
        expires_at,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContactPointDelivery {
    channel: Channel,
    destination: String,
}

fn contact_point_delivery(
    point: &ContactPointInput,
    requested_channel: Option<Channel>,
) -> Result<ContactPointDelivery, HandlerError> {
    let channel = match point.kind {
        ContactPointKind::Email => Channel::Email,
        ContactPointKind::Phone => Channel::Sms,
        ContactPointKind::ExternalId | ContactPointKind::AnonymousHandle => {
            return Err(TerminalError::new_with_code(
                400,
                "contact verification supports email and phone delivery only",
            )
            .into());
        }
    };
    if let Some(requested_channel) = requested_channel
        && requested_channel != channel
    {
        return Err(TerminalError::new_with_code(
            400,
            "delivery channel does not match contact point kind",
        )
        .into());
    }
    Ok(ContactPointDelivery {
        channel,
        destination: normalize_contact_point(point.kind, &point.value)?,
    })
}

async fn consume_contact_verification_challenge(
    pool: &sqlx::PgPool,
    challenge_id: ContactVerificationChallengeId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE contact_verification_challenges
        SET consumed_at = NOW()
        WHERE id = $1 AND consumed_at IS NULL
        "#,
    )
    .bind(challenge_id.0)
    .execute(pool)
    .await?;
    Ok(())
}

async fn complete_contact_verification(
    pool: sqlx::PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
    challenge_id: ContactVerificationChallengeId,
    code: String,
) -> Result<ContactRef, HandlerError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| db_handler_error("begin contact verification completion", error))?;
    let challenge = sqlx::query(
        r#"
        SELECT c.contact_point_id, c.code_hash, c.expires_at, c.consumed_at, c.attempts,
               p.kind, p.normalized_hash, p.display_value
        FROM contact_verification_challenges c
        JOIN contact_points p ON p.id = c.contact_point_id
        WHERE c.id = $1 AND c.contact_id = $2 AND c.tenant_id = $3
        FOR UPDATE
        "#,
    )
    .bind(challenge_id.0)
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| db_handler_error("load contact verification challenge", error))?
    .ok_or_else(|| TerminalError::new_with_code(404, "verification challenge not found"))?;
    let consumed_at = challenge
        .try_get::<Option<chrono::DateTime<Utc>>, _>("consumed_at")
        .map_err(|error| db_handler_error("read challenge consumed_at", error))?;
    if consumed_at.is_some() {
        return Err(
            TerminalError::new_with_code(409, "verification challenge already used").into(),
        );
    }
    let expires_at = challenge
        .try_get::<chrono::DateTime<Utc>, _>("expires_at")
        .map_err(|error| db_handler_error("read challenge expires_at", error))?;
    if expires_at < Utc::now() {
        return Err(TerminalError::new_with_code(410, "verification challenge expired").into());
    }
    let attempts = challenge
        .try_get::<i32, _>("attempts")
        .map_err(|error| db_handler_error("read challenge attempts", error))?;
    if attempts >= MAX_VERIFICATION_ATTEMPTS {
        return Err(
            TerminalError::new_with_code(429, "verification challenge attempts exceeded").into(),
        );
    }
    let stored_hash = challenge
        .try_get::<String, _>("code_hash")
        .map_err(|error| db_handler_error("read challenge code hash", error))?;
    if stored_hash != hash_verification_code(challenge_id, &code) {
        sqlx::query(
            r#"
            UPDATE contact_verification_challenges
            SET attempts = attempts + 1,
                consumed_at = CASE
                    WHEN attempts + 1 >= $2 THEN NOW()
                    ELSE consumed_at
                END
            WHERE id = $1
            "#,
        )
        .bind(challenge_id.0)
        .bind(MAX_VERIFICATION_ATTEMPTS)
        .execute(&mut *transaction)
        .await
        .map_err(|error| db_handler_error("increment verification attempts", error))?;
        transaction
            .commit()
            .await
            .map_err(|error| db_handler_error("commit invalid verification attempt", error))?;
        return Err(TerminalError::new_with_code(403, "invalid verification code").into());
    }

    let point_id = ContactPointId(
        challenge
            .try_get::<Uuid, _>("contact_point_id")
            .map_err(|error| db_handler_error("read challenge contact point", error))?,
    );
    let kind = challenge
        .try_get::<String, _>("kind")
        .map_err(|error| db_handler_error("read contact point kind", error))?;
    let point_kind = parse_contact_point_kind(&kind)?;
    let normalized_hash = challenge
        .try_get::<String, _>("normalized_hash")
        .map_err(|error| db_handler_error("read contact point hash", error))?;
    let display_value = challenge
        .try_get::<Option<String>, _>("display_value")
        .map_err(|error| db_handler_error("read contact point display value", error))?;
    let canonical_id = existing_verified_contact(
        &mut transaction,
        tenant_id,
        point_kind.as_str(),
        &normalized_hash,
        contact_id,
    )
    .await?;

    if let Some(canonical_id) = canonical_id {
        sqlx::query(
            r#"
            UPDATE contacts
            SET state = 'merged', canonical_contact_id = $1, merged_at = NOW(), updated_at = NOW()
            WHERE id = $2 AND tenant_id = $3
            "#,
        )
        .bind(canonical_id.0)
        .bind(contact_id.0)
        .bind(tenant_id.0)
        .execute(&mut *transaction)
        .await
        .map_err(|error| db_handler_error("merge contact", error))?;
    } else {
        sqlx::query(
            r#"
            UPDATE contact_points
            SET verified = TRUE, verified_at = NOW(), updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(point_id.0)
        .execute(&mut *transaction)
        .await
        .map_err(|error| db_handler_error("mark contact point verified", error))?;
        sqlx::query(
            "UPDATE contacts SET state = 'verified', updated_at = NOW() WHERE id = $1 AND tenant_id = $2",
        )
        .bind(contact_id.0)
        .bind(tenant_id.0)
        .execute(&mut *transaction)
        .await
        .map_err(|error| db_handler_error("mark contact verified", error))?;
        upsert_verified_contact_point_channel_account(
            &mut transaction,
            tenant_id,
            contact_id,
            point_id,
            point_kind,
            display_value.as_deref(),
        )
        .await?;
    }

    sqlx::query(
        r#"
        UPDATE contact_token_grants
        SET revoked_at = NOW()
        WHERE contact_id = $1
          AND tenant_id = $2
          AND revoked_at IS NULL
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .execute(&mut *transaction)
    .await
    .map_err(|error| db_handler_error("revoke pre-verification contact token grants", error))?;

    sqlx::query("UPDATE contact_verification_challenges SET consumed_at = NOW() WHERE id = $1")
        .bind(challenge_id.0)
        .execute(&mut *transaction)
        .await
        .map_err(|error| db_handler_error("consume contact verification challenge", error))?;

    transaction
        .commit()
        .await
        .map_err(|error| db_handler_error("commit contact verification completion", error))?;

    load_contact_ref(pool, tenant_id, canonical_id.unwrap_or(contact_id)).await
}

async fn load_contact_ref(
    pool: sqlx::PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
) -> Result<ContactRef, HandlerError> {
    let row = sqlx::query(
        r#"
        SELECT id, tenant_id, state, canonical_contact_id
        FROM contacts
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .fetch_optional(&pool)
    .await
    .map_err(|error| db_handler_error("load contact", error))?
    .ok_or_else(|| TerminalError::new_with_code(404, "contact not found"))?;
    let state = row
        .try_get::<String, _>("state")
        .map_err(|error| db_handler_error("read contact state", error))?;
    Ok(ContactRef {
        contact_id,
        tenant_id,
        state: parse_contact_state(&state)?,
        canonical_contact_id: row
            .try_get::<Option<Uuid>, _>("canonical_contact_id")
            .map_err(|error| db_handler_error("read canonical contact id", error))?
            .map(ContactId),
        linked_contact_ids: Vec::new(),
        scopes: Vec::new(),
        permissions: serde_json::Value::Null,
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    })
}

async fn resolve_contact_session_channel(
    pool: &sqlx::PgPool,
    contact: &ContactRef,
    channel_ref: ChannelRef,
) -> Result<ResolvedSessionChannel, HandlerError> {
    match channel_ref {
        ChannelRef::Chat {
            conversation_id,
            user_id,
            client_session_id,
        } => {
            if conversation_id.trim().is_empty() {
                return Err(
                    TerminalError::new_with_code(400, "chat conversation_id is required").into(),
                );
            }
            let display_name = Some(
                user_id
                    .clone()
                    .unwrap_or_else(|| format!("chat:{conversation_id}")),
            );
            let account = upsert_external_channel_account(
                pool,
                contact,
                Channel::Chat,
                None,
                user_id.as_deref().unwrap_or(conversation_id.as_str()),
                display_name,
            )
            .await?;
            Ok(ResolvedSessionChannel {
                channel_ref: ChannelRef::Chat {
                    conversation_id,
                    user_id,
                    client_session_id,
                },
                channel_account: Some(account),
                contact_point_id: None,
            })
        }
        ChannelRef::Slack {
            team_id,
            slack_channel_id,
            thread_ts,
            user_id,
        } => {
            let user_id = user_id.ok_or_else(|| {
                TerminalError::new_with_code(400, "slack channel route requires user_id")
            })?;
            let account = upsert_external_channel_account(
                pool,
                contact,
                Channel::Slack,
                team_id.as_deref(),
                &user_id,
                Some(format!("<@{user_id}>")),
            )
            .await?;
            Ok(ResolvedSessionChannel {
                channel_ref: ChannelRef::Slack {
                    team_id,
                    slack_channel_id,
                    thread_ts,
                    user_id: Some(user_id),
                },
                channel_account: Some(account),
                contact_point_id: None,
            })
        }
        ChannelRef::Email { channel_account_id } => {
            resolve_contact_point_channel_account(
                pool,
                contact,
                channel_account_id,
                Channel::Email,
                ContactPointKind::Email,
            )
            .await
        }
        ChannelRef::Sms { channel_account_id } => {
            resolve_contact_point_channel_account(
                pool,
                contact,
                channel_account_id,
                Channel::Sms,
                ContactPointKind::Phone,
            )
            .await
        }
    }
}

async fn upsert_external_channel_account(
    pool: &sqlx::PgPool,
    contact: &ContactRef,
    channel: Channel,
    external_tenant_key: Option<&str>,
    external_user_key: &str,
    display_name: Option<String>,
) -> Result<ChannelAccountRef, HandlerError> {
    if external_user_key.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "channel user id is required").into());
    }
    let row = sqlx::query(
        r#"
        SELECT id, contact_id, display_name
        FROM contact_channel_accounts
        WHERE tenant_id = $1
          AND channel = $2
          AND COALESCE(external_tenant_key, '') = COALESCE($3, '')
          AND external_user_key = $4
          AND merged_into_id IS NULL
        "#,
    )
    .bind(contact.tenant_id.0)
    .bind(channel.as_str())
    .bind(external_tenant_key)
    .bind(external_user_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| db_handler_error("load channel account", error))?;

    if let Some(row) = row {
        let account_contact_id = ContactId(
            row.try_get::<Uuid, _>("contact_id")
                .map_err(|error| db_handler_error("read channel account contact", error))?,
        );
        if !contact_allows_channel_contact(contact, account_contact_id) {
            return Err(TerminalError::new_with_code(
                403,
                "channel account belongs to another contact",
            )
            .into());
        }
        let account_id = ChannelAccountId(
            row.try_get::<Uuid, _>("id")
                .map_err(|error| db_handler_error("read channel account id", error))?,
        );
        sqlx::query(
            r#"
            UPDATE contact_channel_accounts
            SET last_seen_at = NOW(), display_name = COALESCE($1, display_name)
            WHERE id = $2
            "#,
        )
        .bind(display_name.as_deref())
        .bind(account_id.0)
        .execute(pool)
        .await
        .map_err(|error| db_handler_error("touch channel account", error))?;
        return Ok(ChannelAccountRef {
            channel_account_id: account_id,
            contact_point_id: None,
            channel,
            display_name: display_name.or_else(|| {
                row.try_get::<Option<String>, _>("display_name")
                    .ok()
                    .flatten()
            }),
        });
    }

    let account_id = ChannelAccountId::new();
    sqlx::query(
        r#"
        INSERT INTO contact_channel_accounts
            (id, tenant_id, workspace_id, contact_id, channel, external_tenant_key,
             external_user_key, display_name, assurance, metadata)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'provider_asserted', $9)
        "#,
    )
    .bind(account_id.0)
    .bind(contact.tenant_id.0)
    .bind(storage_workspace_id_for_tenant(contact.tenant_id).as_str())
    .bind(contact.contact_id.0)
    .bind(channel.as_str())
    .bind(external_tenant_key)
    .bind(external_user_key)
    .bind(display_name.as_deref())
    .bind(serde_json::json!({ "source": "session_channel" }))
    .execute(pool)
    .await
    .map_err(|error| db_handler_error("insert channel account", error))?;
    Ok(ChannelAccountRef {
        channel_account_id: account_id,
        contact_point_id: None,
        channel,
        display_name,
    })
}

async fn resolve_contact_point_channel_account(
    pool: &sqlx::PgPool,
    contact: &ContactRef,
    channel_account_id: ChannelAccountId,
    channel: Channel,
    expected_kind: ContactPointKind,
) -> Result<ResolvedSessionChannel, HandlerError> {
    let row = sqlx::query(
        r#"
        SELECT a.id, a.contact_id, a.contact_point_id, a.display_name,
               p.kind, p.verified
        FROM contact_channel_accounts a
        JOIN contact_points p ON p.id = a.contact_point_id
        WHERE a.id = $1
          AND a.tenant_id = $2
          AND a.channel = $3
          AND a.merged_into_id IS NULL
        "#,
    )
    .bind(channel_account_id.0)
    .bind(contact.tenant_id.0)
    .bind(channel.as_str())
    .fetch_optional(pool)
    .await
    .map_err(|error| db_handler_error("load contact channel account", error))?
    .ok_or_else(|| TerminalError::new_with_code(404, "channel account not found"))?;

    let account_contact_id = ContactId(
        row.try_get::<Uuid, _>("contact_id")
            .map_err(|error| db_handler_error("read channel account contact", error))?,
    );
    if !contact_allows_channel_contact(contact, account_contact_id) {
        return Err(TerminalError::new_with_code(
            403,
            "channel account belongs to another contact",
        )
        .into());
    }
    let point_id = ContactPointId(
        row.try_get::<Uuid, _>("contact_point_id")
            .map_err(|error| db_handler_error("read channel account contact point", error))?,
    );
    let kind = row
        .try_get::<String, _>("kind")
        .map_err(|error| db_handler_error("read channel account contact point kind", error))?;
    if parse_contact_point_kind(&kind)? != expected_kind {
        return Err(TerminalError::new_with_code(
            400,
            "channel account contact point kind mismatch",
        )
        .into());
    }
    let verified = row
        .try_get::<bool, _>("verified")
        .map_err(|error| db_handler_error("read channel account verification", error))?;
    if !verified {
        return Err(TerminalError::new_with_code(
            403,
            "channel account contact point is not verified",
        )
        .into());
    }
    let channel_ref = match channel {
        Channel::Email => ChannelRef::Email { channel_account_id },
        Channel::Sms => ChannelRef::Sms { channel_account_id },
        Channel::Chat | Channel::Slack => {
            return Err(
                TerminalError::new_with_code(400, "unsupported contact point channel").into(),
            );
        }
    };
    Ok(ResolvedSessionChannel {
        channel_ref,
        channel_account: Some(ChannelAccountRef {
            channel_account_id,
            contact_point_id: Some(point_id),
            channel,
            display_name: row
                .try_get::<Option<String>, _>("display_name")
                .map_err(|error| db_handler_error("read channel account display name", error))?,
        }),
        contact_point_id: Some(point_id),
    })
}

fn contact_allows_channel_contact(contact: &ContactRef, account_contact_id: ContactId) -> bool {
    contact.contact_id == account_contact_id
        || contact.canonical_contact_id == Some(account_contact_id)
}

async fn insert_contact_point(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: TenantId,
    contact_id: ContactId,
    point: ContactPointInput,
    verified: bool,
) -> Result<ContactPointRef, HandlerError> {
    let normalized = normalize_contact_point(point.kind, &point.value)?;
    let normalized_hash = hash_contact_point(tenant_id, point.kind, &normalized)?;
    let point_id = ContactPointId::new();
    let verified_at = verified.then(Utc::now);
    let row = sqlx::query(
        r#"
        INSERT INTO contact_points
            (id, contact_id, tenant_id, workspace_id, kind, normalized_hash, display_value, verified, verified_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (tenant_id, workspace_id, contact_id, kind, normalized_hash)
        DO UPDATE SET
            display_value = COALESCE(EXCLUDED.display_value, contact_points.display_value),
            verified = contact_points.verified OR EXCLUDED.verified,
            verified_at = COALESCE(contact_points.verified_at, EXCLUDED.verified_at),
            updated_at = NOW()
        RETURNING id, display_value, verified, verified_at
        "#,
    )
    .bind(point_id.0)
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .bind(storage_workspace_id_for_tenant(tenant_id).as_str())
    .bind(point.kind.as_str())
    .bind(&normalized_hash)
    .bind(point.display_value.as_deref())
    .bind(verified)
    .bind(verified_at)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| db_handler_error("upsert contact point", error))?;
    Ok(ContactPointRef {
        id: ContactPointId(
            row.try_get::<Uuid, _>("id")
                .map_err(|error| db_handler_error("read contact point id", error))?,
        ),
        kind: point.kind,
        display_value: row
            .try_get::<Option<String>, _>("display_value")
            .map_err(|error| db_handler_error("read contact point display value", error))?,
        verified: row
            .try_get::<bool, _>("verified")
            .map_err(|error| db_handler_error("read contact point verified flag", error))?,
        verified_at: row
            .try_get::<Option<DateTime<Utc>>, _>("verified_at")
            .map_err(|error| db_handler_error("read contact point verified_at", error))?,
    })
}

async fn upsert_verified_contact_point_channel_account(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: TenantId,
    contact_id: ContactId,
    point_id: ContactPointId,
    kind: ContactPointKind,
    display_name: Option<&str>,
) -> Result<Option<ChannelAccountRef>, HandlerError> {
    let channel = match kind {
        ContactPointKind::Email => Channel::Email,
        ContactPointKind::Phone => Channel::Sms,
        ContactPointKind::ExternalId | ContactPointKind::AnonymousHandle => return Ok(None),
    };
    let updated = sqlx::query(
        r#"
        UPDATE contact_channel_accounts
        SET assurance = 'otp_verified',
            display_name = COALESCE($1, display_name),
            last_seen_at = NOW()
        WHERE tenant_id = $2
          AND contact_point_id = $3
          AND channel = $4
          AND merged_into_id IS NULL
        RETURNING id, display_name
        "#,
    )
    .bind(display_name)
    .bind(tenant_id.0)
    .bind(point_id.0)
    .bind(channel.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| db_handler_error("update verified channel account", error))?;

    if let Some(row) = updated {
        return Ok(Some(ChannelAccountRef {
            channel_account_id: ChannelAccountId(
                row.try_get::<Uuid, _>("id")
                    .map_err(|error| db_handler_error("read verified channel account id", error))?,
            ),
            contact_point_id: Some(point_id),
            channel,
            display_name: row
                .try_get::<Option<String>, _>("display_name")
                .map_err(|error| {
                    db_handler_error("read verified channel account display", error)
                })?,
        }));
    }

    let account_id = ChannelAccountId::new();
    sqlx::query(
        r#"
        INSERT INTO contact_channel_accounts
            (id, tenant_id, workspace_id, contact_id, contact_point_id, channel,
             external_user_key, display_name, assurance, metadata)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'otp_verified', $9)
        "#,
    )
    .bind(account_id.0)
    .bind(tenant_id.0)
    .bind(storage_workspace_id_for_tenant(tenant_id).as_str())
    .bind(contact_id.0)
    .bind(point_id.0)
    .bind(channel.as_str())
    .bind(point_id.to_string())
    .bind(display_name)
    .bind(serde_json::json!({ "source": "contact_verification" }))
    .execute(&mut **tx)
    .await
    .map_err(|error| db_handler_error("insert verified channel account", error))?;

    Ok(Some(ChannelAccountRef {
        channel_account_id: account_id,
        contact_point_id: Some(point_id),
        channel,
        display_name: display_name.map(ToOwned::to_owned),
    }))
}

async fn ensure_contact_in_tenant(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: TenantId,
    contact_id: ContactId,
) -> Result<(), HandlerError> {
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM contacts WHERE id = $1 AND tenant_id = $2)",
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| db_handler_error("check contact workspace", error))?;
    if exists {
        Ok(())
    } else {
        Err(TerminalError::new_with_code(404, "contact not found").into())
    }
}

async fn validate_contact_session(
    store: &dyn SessionStore,
    session_id: moa_core::SessionId,
    tenant_id: TenantId,
    contact_id: ContactId,
) -> Result<SessionMeta, HandlerError> {
    let meta = store
        .get_session(session_id)
        .await
        .map_err(session_store_handler_error)?;
    if meta.tenant_id != tenant_id {
        return Err(TerminalError::new_with_code(403, "session tenant mismatch").into());
    }
    let Some(contact) = meta.contact.as_ref() else {
        return Err(TerminalError::new_with_code(403, "session has no contact binding").into());
    };
    if contact.contact_id != contact_id {
        return Err(TerminalError::new_with_code(403, "session contact mismatch").into());
    }
    tracing::debug!(
        session_id = %session_id,
        contact_id = %contact_id,
        tenant_id = %tenant_id,
        "validated contact session binding"
    );
    Ok(meta)
}

async fn promoted_from_contact(
    pool: &sqlx::PgPool,
    meta: &SessionMeta,
    contact: &ContactRef,
    tenant_id: TenantId,
) -> Result<Option<ContactId>, HandlerError> {
    let Some(current) = meta.contact.as_ref() else {
        return Err(TerminalError::new_with_code(403, "session has no contact binding").into());
    };
    if current.tenant_id != contact.tenant_id || current.tenant_id != tenant_id {
        return Err(TerminalError::new_with_code(403, "session contact boundary mismatch").into());
    }
    if current.contact_id == contact.contact_id {
        return Ok(None);
    }
    if contact_is_merged_into(pool, tenant_id, current.contact_id, contact.contact_id).await? {
        return Ok(Some(current.contact_id));
    }
    Err(
        TerminalError::new_with_code(403, "session contact is not linked to verified contact")
            .into(),
    )
}

async fn contact_is_merged_into(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
    canonical_contact_id: ContactId,
) -> Result<bool, HandlerError> {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM contacts
            WHERE id = $1
              AND tenant_id = $2
              AND canonical_contact_id = $3
              AND state = 'merged'
        )
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .bind(canonical_contact_id.0)
    .fetch_one(pool)
    .await
    .map_err(|error| db_handler_error("check promoted contact linkage", error))
}

async fn create_contact_token_grant(
    pool: sqlx::PgPool,
    claims: &ContactTokenClaims,
    contact_id: ContactId,
    expires_at: DateTime<Utc>,
    issued_by_actor_type: &'static str,
    issued_by_actor_id: Option<Uuid>,
) -> Result<(), HandlerError> {
    let session_ids = claims
        .session_ids
        .iter()
        .map(|session_id| session_id.0)
        .collect::<Vec<_>>();
    sqlx::query(
        r#"
        INSERT INTO contact_token_grants
            (id, token_jti, tenant_id, workspace_id, contact_id, state, scopes, permissions,
             agent_ids, session_ids, issued_by_actor_type, issued_by_actor_id, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (token_jti) DO NOTHING
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(&claims.jti)
    .bind(claims.tenant_id.0)
    .bind(storage_workspace_id_for_tenant(claims.tenant_id).as_str())
    .bind(contact_id.0)
    .bind(claims.state.as_str())
    .bind(&claims.scopes)
    .bind(&claims.permissions)
    .bind(&claims.agent_ids)
    .bind(&session_ids)
    .bind(issued_by_actor_type)
    .bind(issued_by_actor_id)
    .bind(expires_at)
    .execute(&pool)
    .await
    .map_err(|error| db_handler_error("insert contact token grant", error))?;
    Ok(())
}

async fn ensure_contact_token_grant_active(
    pool: &sqlx::PgPool,
    claims: &ContactTokenClaims,
    contact_id: ContactId,
) -> Result<(), HandlerError> {
    let active = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM contact_token_grants
            WHERE token_jti = $1
              AND tenant_id = $2
              AND contact_id = $3
              AND state = $4
              AND revoked_at IS NULL
              AND expires_at > NOW()
        )
        "#,
    )
    .bind(&claims.jti)
    .bind(claims.tenant_id.0)
    .bind(contact_id.0)
    .bind(claims.state.as_str())
    .fetch_one(pool)
    .await
    .map_err(|error| db_handler_error("check contact token grant", error))?;
    if active {
        Ok(())
    } else {
        Err(TerminalError::new_with_code(401, "contact token grant is not active").into())
    }
}

async fn existing_verified_contact(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: TenantId,
    kind: &str,
    normalized_hash: &str,
    excluded_contact_id: ContactId,
) -> Result<Option<ContactId>, HandlerError> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT contact_id
        FROM contact_points
        WHERE tenant_id = $1
          AND kind = $2
          AND normalized_hash = $3
          AND verified = TRUE
          AND contact_id <> $4
        LIMIT 1
        "#,
    )
    .bind(tenant_id.0)
    .bind(kind)
    .bind(normalized_hash)
    .bind(excluded_contact_id.0)
    .fetch_optional(&mut **tx)
    .await
    .map(|value| value.map(ContactId))
    .map_err(|error| db_handler_error("find existing verified contact", error))
}

fn contact_token_issuer()
-> Result<std::sync::Arc<moa_auth_providers::ContactTokenIssuer>, HandlerError> {
    OrchestratorCtx::current()
        .auth_providers()
        .contact_tokens
        .ok_or_else(|| {
            TerminalError::new_with_code(503, "contact token signing keys are not configured")
                .into()
        })
}

fn annotate_contact_operation_span(contact: &ContactRef, session_id: Option<moa_core::SessionId>) {
    let span = tracing::Span::current();
    span.set_attribute("moa.tenant.id", contact.tenant_id.to_string());
    span.set_attribute("moa.contact.id", contact.contact_id.to_string());
    span.set_attribute("moa.contact.state", contact.state.as_str().to_string());
    if let Some(session_id) = session_id {
        span.set_attribute("moa.session.id", session_id.to_string());
    }
}

fn annotate_claim_contact_span(
    claims: &ContactTokenClaims,
    session_id: Option<moa_core::SessionId>,
) {
    let span = tracing::Span::current();
    span.set_attribute("moa.tenant.id", claims.tenant_id.to_string());
    span.set_attribute("moa.contact.id", claims.sub.clone());
    span.set_attribute("moa.contact.state", claims.state.as_str().to_string());
    if let Some(session_id) = session_id {
        span.set_attribute("moa.session.id", session_id.to_string());
    }
}

async fn authorize_tenant_operator(
    identity: &Identity,
    tenant_id: TenantId,
) -> Result<(), HandlerError> {
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Operator,
    )
    .await
    .map_err(translate_authz_error)
}

fn verify_contact_token(
    token: &str,
    tenant_id: TenantId,
) -> Result<ContactTokenClaims, HandlerError> {
    let claims = contact_token_issuer()?
        .verify(token)
        .map_err(contact_token_handler_error)?;
    if claims.tenant_id != tenant_id {
        return Err(TerminalError::new_with_code(403, "contact token tenant mismatch").into());
    }
    Ok(claims)
}

fn require_contact_scope(
    claims: &ContactTokenClaims,
    required_scope: &str,
) -> Result<(), HandlerError> {
    if claims.scopes.iter().any(|scope| scope == required_scope) {
        Ok(())
    } else {
        Err(TerminalError::new_with_code(403, "contact token scope denied").into())
    }
}

fn require_contact_session_permission(
    claims: &ContactTokenClaims,
    session_id: Option<moa_core::SessionId>,
) -> Result<(), HandlerError> {
    let Some(session_id) = session_id else {
        return Ok(());
    };
    if claims.session_ids.is_empty() || claims.session_ids.contains(&session_id) {
        Ok(())
    } else {
        Err(TerminalError::new_with_code(403, "contact token session denied").into())
    }
}

fn require_contact_agent_permission(
    claims: &ContactTokenClaims,
    agent: &AgentSessionSelection,
) -> Result<(), HandlerError> {
    if claims.agent_ids.is_empty() {
        validate_contact_agent_selection(agent).map(|_| ())
    } else {
        let selected_agent = validate_contact_agent_selection(agent)?;
        if claims
            .agent_ids
            .iter()
            .any(|agent_id| agent_id == &selected_agent)
        {
            Ok(())
        } else {
            Err(TerminalError::new_with_code(403, "contact token agent denied").into())
        }
    }
}

fn validate_contact_agent_selection(agent: &AgentSessionSelection) -> Result<String, HandlerError> {
    match (agent.installation_uid, agent.revision_uid) {
        (Some(installation_uid), None) => Ok(installation_uid.to_string()),
        (None, Some(revision_uid)) => Ok(revision_uid.to_string()),
        _ => Err(TerminalError::new_with_code(
            400,
            "contact session requires exactly one agent installation_uid or revision_uid",
        )
        .into()),
    }
}

fn contact_id_from_claims(claims: &ContactTokenClaims) -> Result<ContactId, HandlerError> {
    Uuid::parse_str(&claims.sub)
        .map(ContactId)
        .map_err(|_| TerminalError::new_with_code(400, "contact token subject is invalid").into())
}

fn low_assurance_scopes(requested_scopes: &[String]) -> Vec<String> {
    bounded_scopes(requested_scopes, LOW_ASSURANCE_SCOPES)
}

fn verified_scopes() -> Vec<String> {
    VERIFIED_SCOPES
        .iter()
        .map(|scope| (*scope).to_string())
        .collect()
}

fn bounded_scopes(requested_scopes: &[String], allowed: &[&str]) -> Vec<String> {
    if requested_scopes.is_empty() {
        return allowed.iter().map(|scope| (*scope).to_string()).collect();
    }
    requested_scopes
        .iter()
        .filter(|scope| allowed.iter().any(|allowed| allowed == &scope.as_str()))
        .cloned()
        .collect()
}

fn normalize_contact_point(kind: ContactPointKind, value: &str) -> Result<String, HandlerError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(TerminalError::new_with_code(400, "contact point value is required").into());
    }
    match kind {
        ContactPointKind::Email => {
            let normalized = trimmed.to_ascii_lowercase();
            if !normalized.contains('@') {
                return Err(
                    TerminalError::new_with_code(400, "invalid email contact point").into(),
                );
            }
            Ok(normalized)
        }
        ContactPointKind::Phone => normalize_phone(trimmed),
        ContactPointKind::ExternalId | ContactPointKind::AnonymousHandle => Ok(trimmed.to_string()),
    }
}

fn normalize_phone(value: &str) -> Result<String, HandlerError> {
    let digits: String = value.chars().filter(char::is_ascii_digit).collect();
    if !(8..=15).contains(&digits.len()) {
        return Err(TerminalError::new_with_code(400, "invalid phone contact point").into());
    }
    Ok(format!("+{digits}"))
}

fn hash_contact_point(
    tenant_id: TenantId,
    kind: ContactPointKind,
    normalized: &str,
) -> Result<String, HandlerError> {
    let key_env = OrchestratorCtx::current_config()
        .auth
        .contact_tokens
        .contact_point_hash_key_env
        .clone();
    let key_hex = std::env::var(&key_env).map_err(|_| {
        TerminalError::new_with_code(503, "contact point hash key is not configured")
    })?;
    let key_bytes = hex::decode(key_hex.trim()).map_err(|error| {
        TerminalError::new_with_code(
            503,
            format!("contact point hash key must be hex-encoded: {error}"),
        )
    })?;
    let key: [u8; 32] = key_bytes.try_into().map_err(|bytes: Vec<u8>| {
        TerminalError::new_with_code(
            503,
            format!(
                "contact point hash key must be 32 bytes, got {}",
                bytes.len()
            ),
        )
    })?;
    Ok(blake3::keyed_hash(
        &key,
        format!("{tenant_id}:{}:{normalized}", kind.as_str()).as_bytes(),
    )
    .to_hex()
    .to_string())
}

fn verification_code() -> String {
    format!("{:06}", rand::thread_rng().gen_range(0..1_000_000))
}

fn hash_verification_code(challenge_id: ContactVerificationChallengeId, code: &str) -> String {
    blake3::hash(format!("{challenge_id}:{}", code.trim()).as_bytes())
        .to_hex()
        .to_string()
}

fn parse_contact_state(value: &str) -> Result<ContactVerificationState, HandlerError> {
    value
        .parse::<ContactVerificationState>()
        .map_err(|_| TerminalError::new_with_code(500, "invalid stored contact state").into())
}

fn parse_contact_point_kind(value: &str) -> Result<ContactPointKind, HandlerError> {
    value
        .parse::<ContactPointKind>()
        .map_err(|_| TerminalError::new_with_code(500, "invalid stored contact point kind").into())
}

fn contact_token_handler_error(error: moa_auth_providers::ContactTokenError) -> HandlerError {
    match error {
        moa_auth_providers::ContactTokenError::Expired => {
            TerminalError::new_with_code(401, "contact token expired").into()
        }
        moa_auth_providers::ContactTokenError::InvalidFormat
        | moa_auth_providers::ContactTokenError::Rejected => {
            TerminalError::new_with_code(401, "invalid contact token").into()
        }
        moa_auth_providers::ContactTokenError::MissingEnv(_)
        | moa_auth_providers::ContactTokenError::InvalidKey(_) => {
            TerminalError::new_with_code(503, "contact token provider unavailable").into()
        }
    }
}

fn session_store_handler_error(error: MoaError) -> HandlerError {
    match error {
        MoaError::SessionNotFound(_) => {
            TerminalError::new_with_code(404, "session not found").into()
        }
        error => TerminalError::new(format!("session store error: {error}")).into(),
    }
}

fn contact_delivery_handler_error(error: MoaError) -> HandlerError {
    let error_kind = match &error {
        MoaError::ConfigError(_) | MoaError::MissingEnvironmentVariable(_) => "configuration",
        MoaError::ValidationError(_) => "validation",
        MoaError::RateLimited { .. } => "rate_limited",
        MoaError::HttpStatus { status, .. } if (500..600).contains(status) => "provider_5xx",
        MoaError::HttpStatus { .. } => "provider_http",
        MoaError::ProviderQuirk(_) => "provider_retryable",
        MoaError::ProviderError(_) => "provider",
        _ => "other",
    };
    tracing::warn!(
        error_kind,
        "contact delivery failed before verification challenge could be used"
    );
    match error {
        MoaError::ConfigError(_) | MoaError::MissingEnvironmentVariable(_) => {
            TerminalError::new_with_code(503, "contact delivery provider is not configured").into()
        }
        MoaError::ValidationError(_) => {
            TerminalError::new_with_code(400, "contact delivery request is invalid").into()
        }
        MoaError::RateLimited { .. } => {
            TerminalError::new_with_code(429, "contact delivery provider is rate limited").into()
        }
        MoaError::HttpStatus { status, .. } if (500..600).contains(&status) => {
            TerminalError::new_with_code(502, "contact delivery provider failed").into()
        }
        _ => TerminalError::new_with_code(502, "contact delivery provider failed").into(),
    }
}

fn db_handler_error(context: &'static str, error: sqlx::Error) -> HandlerError {
    TerminalError::new(format!("{context}: {error}")).into()
}

#[cfg(test)]
mod tests {
    use moa_core::{
        AgentSessionSelection, Channel, ContactId, ContactPointInput, ContactPointKind, ContactRef,
        ContactTokenClaims, ContactVerificationState, TenantId,
    };

    use super::{
        contact_allows_channel_contact, contact_point_delivery, require_contact_agent_permission,
    };

    #[test]
    fn contact_agent_permission_allows_unbounded_token_with_single_selector() {
        // Pins: unbounded contact tokens may create sessions only when exactly one agent selector is provided.
        let installation_uid = uuid::Uuid::now_v7();
        let claims = contact_claims(Vec::new());
        let selection = AgentSessionSelection {
            installation_uid: Some(installation_uid),
            revision_uid: None,
        };

        require_contact_agent_permission(&claims, &selection)
            .expect("unbounded token should allow a single selected agent");
    }

    #[test]
    fn contact_agent_permission_rejects_token_agent_allowlist_miss() {
        // Pins: bounded contact tokens cannot create sessions for agents outside their allowlist.
        let allowed_installation_uid = uuid::Uuid::now_v7();
        let denied_installation_uid = uuid::Uuid::now_v7();
        let claims = contact_claims(vec![allowed_installation_uid.to_string()]);
        let selection = AgentSessionSelection {
            installation_uid: Some(denied_installation_uid),
            revision_uid: None,
        };

        let error = require_contact_agent_permission(&claims, &selection)
            .expect_err("unlisted agent should be denied");

        assert!(
            format!("{error:?}").contains("contact token agent denied"),
            "unexpected error: {error:?}"
        );
    }

    fn contact(
        tenant_id: TenantId,
        contact_id: ContactId,
        linked_contact_ids: Vec<ContactId>,
    ) -> ContactRef {
        ContactRef {
            contact_id,
            tenant_id,
            state: ContactVerificationState::Verified,
            canonical_contact_id: None,
            linked_contact_ids,
            scopes: Vec::new(),
            permissions: serde_json::Value::Null,
            agent_ids: Vec::new(),
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        }
    }

    fn contact_claims(agent_ids: Vec<String>) -> ContactTokenClaims {
        ContactTokenClaims {
            iss: "moa-test".to_string(),
            aud: "moa-contact".to_string(),
            sub: ContactId::new().to_string(),
            exp: 1,
            iat: 0,
            nbf: 0,
            jti: uuid::Uuid::now_v7().to_string(),
            tenant_id: TenantId::from(uuid::Uuid::now_v7()),
            state: ContactVerificationState::Unverified,
            scopes: vec!["agent:session:create".to_string()],
            permissions: serde_json::Value::Null,
            agent_ids,
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
            linked_contact_ids: Vec::new(),
        }
    }

    #[test]
    fn contact_point_delivery_routes_email_and_phone_only() {
        // Pins: OTP delivery supports email and SMS contact points, not external ids or anonymous handles.
        let email = contact_point_delivery(
            &ContactPointInput {
                kind: ContactPointKind::Email,
                value: "USER@EXAMPLE.COM".to_string(),
                display_value: None,
            },
            None,
        )
        .expect("email contact point should support delivery");
        assert_eq!(email.channel, Channel::Email);
        assert_eq!(email.destination, "user@example.com");

        let phone = contact_point_delivery(
            &ContactPointInput {
                kind: ContactPointKind::Phone,
                value: "(500) 555-0006".to_string(),
                display_value: None,
            },
            Some(Channel::Sms),
        )
        .expect("phone contact point should support SMS delivery");
        assert_eq!(phone.channel, Channel::Sms);
        assert_eq!(phone.destination, "+5005550006");

        let mismatch = contact_point_delivery(
            &ContactPointInput {
                kind: ContactPointKind::Email,
                value: "user@example.com".to_string(),
                display_value: None,
            },
            Some(Channel::Sms),
        )
        .expect_err("email contact point should reject SMS delivery");
        let mismatch = format!("{mismatch:?}");
        assert!(
            mismatch.contains("delivery channel"),
            "unexpected mismatch error: {mismatch}"
        );

        let external = contact_point_delivery(
            &ContactPointInput {
                kind: ContactPointKind::ExternalId,
                value: "customer-123".to_string(),
                display_value: None,
            },
            None,
        )
        .expect_err("external id should not support OTP delivery");
        let external = format!("{external:?}");
        assert!(
            external.contains("email and phone"),
            "unexpected external-id error: {external}"
        );
    }

    #[test]
    fn contact_allows_channel_accounts_for_self_and_canonical_contacts_only() {
        // Pins: channel-account validation does not follow linked contacts by default.
        let tenant_id = TenantId::from(uuid::Uuid::now_v7());
        let contact_id = ContactId::new();
        let canonical_id = ContactId::new();
        let linked_id = ContactId::new();
        let unrelated_id = ContactId::new();
        let mut contact = contact(tenant_id, contact_id, vec![linked_id]);
        contact.canonical_contact_id = Some(canonical_id);

        assert!(contact_allows_channel_contact(&contact, contact_id));
        assert!(contact_allows_channel_contact(&contact, canonical_id));
        assert!(!contact_allows_channel_contact(&contact, linked_id));
        assert!(!contact_allows_channel_contact(&contact, unrelated_id));
    }
}
