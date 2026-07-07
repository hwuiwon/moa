//! Restate service for agent-facing contact identity operations.

use moa_authz::{enqueue, enqueue_raw};
use moa_authz_schema::{ObjectType, Relation, TupleKey, TupleOp, UserType};
use moa_contacts::ContactError;
use moa_contacts::domain::{
    contact_id_from_claims, low_assurance_scopes, require_contact_agent_allowlist,
    require_contact_agent_permission, require_contact_scope, require_contact_session_permission,
    verified_scopes,
};
use moa_contacts::repository::{
    ContactVerificationStartCommand, complete_contact_verification, create_contact_token_grant,
    ensure_contact_token_grant_active, issue_contact, load_contact_ref, promoted_from_contact,
    resolve_contact_session_channel, start_contact_verification,
};
use moa_core::traits::{Identity, IdentityType, SessionChannelBindingUpdate};
use moa_core::wire::turn::{QueueMessageRequest, SessionProgress, SessionProgressRequest};
use moa_core::{
    ChannelAccountRef, ChannelRef, ContactId, ContactPointId, ContactRef,
    ContactSessionAuthorizationRequest, ContactSessionAuthorizationResponse,
    ContactSessionChannelChangeRequest, ContactSessionChannelChangeResponse,
    ContactSessionInitRequest, ContactSessionInitResponse, ContactSessionMessageRequest,
    ContactSessionMessageResponse, ContactSessionProgressRequest, ContactSessionPromotionRequest,
    ContactSessionPromotionResponse, ContactTokenClaims, ContactTokenIssueRequest,
    ContactTokenIssueResponse, ContactVerificationCompleteRequest,
    ContactVerificationCompleteResponse, ContactVerificationStartRequest,
    ContactVerificationStartResponse, Event, ModelId, SessionActorRef, SessionMeta, SessionStatus,
    StoragePartitionId, TenantId,
};
use moa_core::{MoaError, SessionStore};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::authorize_tenant;
use crate::objects::session::SessionClient;
use crate::restate_identity::with_identity_headers;
use crate::services::session_store::inner::{
    create_session_for_identity, resolve_agent_context_for_session,
};

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

    /// Sends one user message to an existing contact-owned session.
    async fn send_message(
        request: Json<ContactSessionMessageRequest>,
    ) -> Result<Json<ContactSessionMessageResponse>, HandlerError>;

    /// Authorizes access to an existing contact-owned session.
    async fn authorize_session(
        request: Json<ContactSessionAuthorizationRequest>,
    ) -> Result<Json<ContactSessionAuthorizationResponse>, HandlerError>;

    /// Returns progress for an existing contact-owned session.
    async fn progress(
        request: Json<ContactSessionProgressRequest>,
    ) -> Result<Json<SessionProgress>, HandlerError>;

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
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let tenant_id = request.tenant_id;
        let token_issuer = contact_token_issuer()?;
        let pool = OrchestratorCtx::current_graph_pool();
        let contact_point_hash_key_hex = OrchestratorCtx::current_config()
            .auth
            .contact_tokens
            .contact_point_hash_key_hex
            .clone();
        let requested_scopes = request.requested_scopes.clone();
        let granted_scopes =
            low_assurance_scopes(&requested_scopes).map_err(contact_error_handler_error)?;
        require_contact_agent_allowlist(&request.agent_ids).map_err(contact_error_handler_error)?;

        let (contact, contact_points) = ctx
            .run(|| async move {
                issue_contact(pool, &contact_point_hash_key_hex, tenant_id, request)
                    .await
                    .map_err(contact_error_handler_error)
                    .map(Json::from)
            })
            .name("contacts_issue_contact")
            .await?
            .into_inner();
        let contact = contact.with_scopes(granted_scopes);
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
            .map_err(contact_error_handler_error)
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
        require_contact_scope(&claims, "contact:verify:start")
            .map_err(contact_error_handler_error)?;
        require_contact_session_permission(&claims, request.session_id)
            .map_err(contact_error_handler_error)?;
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
        annotate_claim_contact_span(&claims, request.session_id);
        let tenant_id = claims.tenant_id;
        let pool = OrchestratorCtx::current_graph_pool();
        let store = OrchestratorCtx::current_session_store();
        let config = OrchestratorCtx::current_config();
        let ttl_seconds = config.auth.contact_tokens.verification_ttl_seconds;
        let contact_point_hash_key_hex = config
            .auth
            .contact_tokens
            .contact_point_hash_key_hex
            .clone();
        let messaging_config = config.messaging.clone();
        let delivery_channel = request.delivery_channel;
        let contact_point = request.contact_point;
        let session_id = request.session_id;

        Ok(ctx
            .run(|| async move {
                ensure_contact_token_grant_active(&pool, &claims, contact_id)
                    .await
                    .map_err(contact_error_handler_error)?;
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
                        contact_point_hash_key_hex,
                        messaging_config,
                    },
                )
                .await
                .map_err(contact_error_handler_error)
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
        require_contact_scope(&claims, "contact:verify:complete")
            .map_err(contact_error_handler_error)?;
        require_contact_session_permission(&claims, request.session_id)
            .map_err(contact_error_handler_error)?;
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
        annotate_claim_contact_span(&claims, request.session_id);
        let tenant_id = claims.tenant_id;
        let token_issuer = contact_token_issuer()?;
        let pool = OrchestratorCtx::current_graph_pool();
        let store = OrchestratorCtx::current_session_store();
        let session_id = request.session_id;

        let contact = ctx
            .run(|| async move {
                ensure_contact_token_grant_active(&pool, &claims, contact_id)
                    .await
                    .map_err(contact_error_handler_error)?;
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
                .map_err(contact_error_handler_error)
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
            .map_err(contact_error_handler_error)
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
        require_contact_scope(&claims, "agent:session:create")
            .map_err(contact_error_handler_error)?;
        require_contact_agent_permission(&claims, &request.agent)
            .map_err(contact_error_handler_error)?;
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
        annotate_claim_contact_span(&claims, None);
        let tenant_id = claims.tenant_id;
        let pool = OrchestratorCtx::current_graph_pool();
        let store = OrchestratorCtx::current_session_store();
        let store_backend = OrchestratorCtx::current().session_store_backend();
        let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
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
        let prepare_pool = pool.clone();

        let SessionChannelPreparation {
            contact,
            channel_ref,
            channel_account,
            contact_point_id,
        } = ctx
            .run(|| async move {
                ensure_contact_token_grant_active(&prepare_pool, &claims, contact_id)
                    .await
                    .map_err(contact_error_handler_error)?;
                let mut contact = load_contact_ref(prepare_pool.clone(), tenant_id, contact_id)
                    .await
                    .map_err(contact_error_handler_error)?;
                contact.scopes = token_scopes;
                contact.permissions = token_permissions;
                contact.agent_ids = token_agent_ids;
                contact.session_ids = token_session_ids;
                contact.verified_contact_point_ids = token_verified_contact_point_ids;
                contact.linked_contact_ids = token_linked_contact_ids;
                let resolved =
                    resolve_contact_session_channel(&prepare_pool, &contact, initial_channel_ref)
                        .await
                        .map_err(contact_error_handler_error)?;
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
        let storage_partition_id_for_create = storage_partition_id.clone();
        let resolver_pool = pool.clone();
        let create_pool = pool.clone();
        let (session_id, meta_for_vo) = ctx
            .run(|| async move {
                let agent_context =
                    resolve_agent_context_for_session(resolver_pool, &meta, &agent_selection)
                        .await?;
                let mut meta = meta;
                meta.agent_context = Some(agent_context);
                let identity = contact_identity(contact.contact_id, tenant_id);
                let session_id = create_session_for_identity(
                    store_backend.as_ref(),
                    &create_pool,
                    meta,
                    identity,
                )
                .await?;
                store
                    .replace_session_channel_binding(SessionChannelBindingUpdate {
                        tenant_id,
                        storage_partition_id: storage_partition_id_for_create,
                        session_id,
                        contact_id: contact.contact_id,
                        channel_account_id: channel_account
                            .as_ref()
                            .map(|account| account.channel_account_id),
                        contact_point_id,
                        channel_ref,
                        reason: channel_reason,
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
        require_contact_scope(&claims, "contact:session:channel:update")
            .map_err(contact_error_handler_error)?;
        require_contact_session_permission(&claims, Some(request.session_id))
            .map_err(contact_error_handler_error)?;
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
        annotate_claim_contact_span(&claims, Some(request.session_id));
        let session_id = request.session_id;
        let tenant_id = claims.tenant_id;
        let pool = OrchestratorCtx::current_graph_pool();
        let store = OrchestratorCtx::current_session_store();
        let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);

        let ChannelChangeResult {
            contact,
            channel_ref,
            channel_account,
            meta,
        } = ctx
            .run(|| async move {
                ensure_contact_token_grant_active(&pool, &claims, contact_id)
                    .await
                    .map_err(contact_error_handler_error)?;
                let existing_meta = validate_contact_session(
                    store.as_ref(),
                    session_id,
                    request.tenant_id,
                    contact_id,
                )
                .await?;
                let contact = load_contact_ref(pool.clone(), tenant_id, contact_id)
                    .await
                    .map_err(contact_error_handler_error)?;
                let resolved =
                    resolve_contact_session_channel(&pool, &contact, request.channel_ref)
                        .await
                        .map_err(contact_error_handler_error)?;
                let binding_id = store
                    .replace_session_channel_binding(SessionChannelBindingUpdate {
                        tenant_id,
                        storage_partition_id,
                        session_id,
                        contact_id: contact.contact_id,
                        channel_account_id: resolved
                            .channel_account
                            .as_ref()
                            .map(|account| account.channel_account_id),
                        contact_point_id: resolved.contact_point_id,
                        channel_ref: resolved.channel_ref.clone(),
                        reason: request.reason.clone(),
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
    // SAFETY: contact JWT verification, scope checks, and session ownership validation bind this message to one contact session.
    async fn send_message(
        &self,
        ctx: Context<'_>,
        request: Json<ContactSessionMessageRequest>,
    ) -> Result<Json<ContactSessionMessageResponse>, HandlerError> {
        annotate_restate_handler_span("Contacts", "send_message");
        let request = request.into_inner();
        if let Err(message) = request.validate_admitted_payload() {
            return Err(TerminalError::new_with_code(400, message).into());
        }
        let claims = verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "contact:session:message:send")
            .map_err(contact_error_handler_error)?;
        require_contact_session_permission(&claims, Some(request.session_id))
            .map_err(contact_error_handler_error)?;
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
        annotate_claim_contact_span(&claims, Some(request.session_id));
        let session_id = request.session_id;
        let tenant_id = claims.tenant_id;
        let pool = OrchestratorCtx::current_graph_pool();
        let store = OrchestratorCtx::current_session_store();

        let contact = ctx
            .run(|| async move {
                ensure_contact_token_grant_active(&pool, &claims, contact_id)
                    .await
                    .map_err(contact_error_handler_error)?;
                let meta =
                    validate_contact_session(store.as_ref(), session_id, tenant_id, contact_id)
                        .await?;
                let Some(contact) = meta.contact else {
                    return Err(TerminalError::new_with_code(
                        403,
                        "session has no contact binding",
                    )
                    .into());
                };
                Ok::<_, HandlerError>(Json::from(contact))
            })
            .name("contacts_validate_message_session")
            .await?
            .into_inner();
        let identity = contact_identity(contact.contact_id, contact.tenant_id);
        let response = with_identity_headers(
            ctx.object_client::<SessionClient>(session_id.to_string())
                .queue_message(Json::from(QueueMessageRequest {
                    user_message: request.user_message,
                    attachments: request.attachments,
                    model: request.model,
                    contact: None,
                    max_turns: request.max_turns,
                })),
            &identity,
        )
        .call()
        .await?
        .into_inner();
        annotate_contact_operation_span(&contact, Some(session_id));

        Ok(Json::from(ContactSessionMessageResponse {
            session_id,
            queued: response.queued,
            started_turn_id: response.started_turn_id,
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: contact JWT verification, scope checks, and session ownership validation bind this authorization to one contact session.
    async fn authorize_session(
        &self,
        ctx: Context<'_>,
        request: Json<ContactSessionAuthorizationRequest>,
    ) -> Result<Json<ContactSessionAuthorizationResponse>, HandlerError> {
        annotate_restate_handler_span("Contacts", "authorize_session");
        let request = request.into_inner();
        let claims = verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "contact:session:message:send")
            .map_err(contact_error_handler_error)?;
        require_contact_session_permission(&claims, Some(request.session_id))
            .map_err(contact_error_handler_error)?;
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
        annotate_claim_contact_span(&claims, Some(request.session_id));
        let session_id = request.session_id;
        let tenant_id = claims.tenant_id;
        let pool = OrchestratorCtx::current_graph_pool();
        let store = OrchestratorCtx::current_session_store();

        let contact = ctx
            .run(|| async move {
                ensure_contact_token_grant_active(&pool, &claims, contact_id)
                    .await
                    .map_err(contact_error_handler_error)?;
                let meta =
                    validate_contact_session(store.as_ref(), session_id, tenant_id, contact_id)
                        .await?;
                let Some(contact) = meta.contact else {
                    return Err(TerminalError::new_with_code(
                        403,
                        "session has no contact binding",
                    )
                    .into());
                };
                Ok::<_, HandlerError>(Json::from(contact))
            })
            .name("contacts_authorize_session")
            .await?
            .into_inner();
        annotate_contact_operation_span(&contact, Some(session_id));

        Ok(Json::from(ContactSessionAuthorizationResponse {
            session_id,
            contact,
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: contact JWT verification, scope checks, and session ownership validation bind this progress read to one contact session.
    async fn progress(
        &self,
        ctx: Context<'_>,
        request: Json<ContactSessionProgressRequest>,
    ) -> Result<Json<SessionProgress>, HandlerError> {
        annotate_restate_handler_span("Contacts", "progress");
        let request = request.into_inner();
        let claims = verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "contact:session:message:send")
            .map_err(contact_error_handler_error)?;
        require_contact_session_permission(&claims, Some(request.session_id))
            .map_err(contact_error_handler_error)?;
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
        annotate_claim_contact_span(&claims, Some(request.session_id));
        let session_id = request.session_id;
        let tenant_id = claims.tenant_id;
        let pool = OrchestratorCtx::current_graph_pool();
        let store = OrchestratorCtx::current_session_store();

        let contact = ctx
            .run(|| async move {
                ensure_contact_token_grant_active(&pool, &claims, contact_id)
                    .await
                    .map_err(contact_error_handler_error)?;
                let meta =
                    validate_contact_session(store.as_ref(), session_id, tenant_id, contact_id)
                        .await?;
                let Some(contact) = meta.contact else {
                    return Err(TerminalError::new_with_code(
                        403,
                        "session has no contact binding",
                    )
                    .into());
                };
                Ok::<_, HandlerError>(Json::from(contact))
            })
            .name("contacts_validate_progress_session")
            .await?
            .into_inner();
        let identity = contact_identity(contact.contact_id, contact.tenant_id);
        let progress = with_identity_headers(
            ctx.object_client::<SessionClient>(session_id.to_string())
                .progress(Json::from(SessionProgressRequest {
                    event_range: request.event_range,
                })),
            &identity,
        )
        .call()
        .await?;
        annotate_contact_operation_span(&contact, Some(session_id));

        Ok(progress)
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
        require_contact_scope(&claims, "contact:session:promote")
            .map_err(contact_error_handler_error)?;
        require_contact_session_permission(&claims, Some(request.session_id))
            .map_err(contact_error_handler_error)?;
        if !claims.state.is_verified() {
            return Err(
                TerminalError::new_with_code(403, "verified contact token required").into(),
            );
        }
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
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
                ensure_contact_token_grant_active(&pool, &claims, contact_id)
                    .await
                    .map_err(contact_error_handler_error)?;
                let contact = load_contact_ref(pool.clone(), tenant_id, contact_id)
                    .await
                    .map_err(contact_error_handler_error)?;
                let meta = store
                    .get_session(request.session_id)
                    .await
                    .map_err(session_store_handler_error)?;
                if meta.tenant_id != request.tenant_id {
                    return Err(TerminalError::new_with_code(403, "session tenant mismatch").into());
                }
                let promoted_from = promoted_from_contact(&pool, &meta, &contact, tenant_id)
                    .await
                    .map_err(contact_error_handler_error)?;
                store
                    .update_session_contact(request.session_id, contact.clone(), promoted_from)
                    .await
                    .map_err(session_store_handler_error)?;
                replace_contact_session_authz_tuples(
                    &pool,
                    tenant_id,
                    request.session_id,
                    promoted_from,
                    contact.contact_id,
                )
                .await?;
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

/// Replaces contact-owned session tuples when a session is promoted to a canonical contact.
pub async fn replace_contact_session_authz_tuples(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    session_id: moa_core::SessionId,
    promoted_from: Option<ContactId>,
    promoted_to: ContactId,
) -> Result<(), HandlerError> {
    let Some(promoted_from) = promoted_from.filter(|contact_id| *contact_id != promoted_to) else {
        return Ok(());
    };
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;

    enqueue_contact_session_owner_tuple(
        &mut transaction,
        TupleOp::Delete,
        tenant_id,
        session_id,
        promoted_from,
    )
    .await?;
    enqueue_contact_session_participant_tuple(
        &mut transaction,
        TupleOp::Delete,
        tenant_id,
        session_id,
        promoted_from,
    )
    .await?;
    enqueue_contact_session_owner_tuple(
        &mut transaction,
        TupleOp::Write,
        tenant_id,
        session_id,
        promoted_to,
    )
    .await?;
    enqueue_contact_session_participant_tuple(
        &mut transaction,
        TupleOp::Write,
        tenant_id,
        session_id,
        promoted_to,
    )
    .await?;

    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    Ok(())
}

async fn enqueue_contact_session_owner_tuple(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    op: TupleOp,
    tenant_id: TenantId,
    session_id: moa_core::SessionId,
    contact_id: ContactId,
) -> Result<(), HandlerError> {
    let owner_tuple = TupleKey::new(
        UserType::Contact,
        contact_id.0,
        Relation::Owner,
        ObjectType::Session,
        session_id.0,
    );
    enqueue(&mut **transaction, op, &owner_tuple, Some(tenant_id.0))
        .await
        .map_err(|error| TerminalError::new(format!("authz outbox owner tuple: {error}")).into())
}

async fn enqueue_contact_session_participant_tuple(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    op: TupleOp,
    tenant_id: TenantId,
    session_id: moa_core::SessionId,
    contact_id: ContactId,
) -> Result<(), HandlerError> {
    enqueue_raw(
        &mut **transaction,
        op,
        &format!("contact:{contact_id}"),
        "contact",
        &format!("session:{session_id}"),
        Some(tenant_id.0),
    )
    .await
    .map_err(|error| TerminalError::new(format!("authz outbox contact tuple: {error}")).into())
}

fn contact_identity(contact_id: ContactId, tenant_id: TenantId) -> Identity {
    Identity {
        identity_type: IdentityType::Contact,
        id: contact_id.0,
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
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

fn contact_error_handler_error(error: ContactError) -> HandlerError {
    match error {
        ContactError::Terminal { code, message } => {
            TerminalError::new_with_code(code, message).into()
        }
        ContactError::SessionStore(MoaError::SessionNotFound(_)) => {
            TerminalError::new_with_code(404, "session not found").into()
        }
        error => TerminalError::new(error.to_string()).into(),
    }
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
