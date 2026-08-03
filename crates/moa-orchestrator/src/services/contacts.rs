//! Restate service for agent-facing contact identity operations.

use moa_authz::{enqueue, enqueue_raw};
use moa_authz_schema::{ObjectType, Relation, TupleKey, TupleOp, UserType};
use moa_config::MoaConfig;
use moa_contacts::Error;
use moa_contacts::domain::{
    contact_id_from_claims, low_assurance_scopes, require_contact_agent_allowlist,
    require_contact_agent_permission, require_contact_scope, require_contact_session_permission,
    verified_scopes,
};
use moa_contacts::repository::{
    complete_contact_verification, create_contact_token_grant, ensure_contact_token_grant_active,
    issue_contact, load_contact_ref, promoted_from_contact, resolve_contact_session_channel,
};
use moa_contacts::verification_service::{ContactVerificationStartCommand, ContactVerifier};
use moa_core::traits::{Identity, IdentityType, SessionChannelBindingUpdate};
use moa_core::{error::MoaError, traits::SessionStore};
use moa_core::{
    events::Event, types::channel::ChannelAccountRef, types::channel::ChannelRef,
    types::channel::SessionChannelBindingId, types::contact::ContactId,
    types::contact::ContactPointId, types::contact::ContactRef,
    types::contact::ContactSessionAuthorizationRequest,
    types::contact::ContactSessionAuthorizationResponse,
    types::contact::ContactSessionChannelChangeRequest,
    types::contact::ContactSessionChannelChangeResponse, types::contact::ContactSessionInitRequest,
    types::contact::ContactSessionInitResponse, types::contact::ContactSessionMessageRequest,
    types::contact::ContactSessionMessageResponse, types::contact::ContactSessionProgressRequest,
    types::contact::ContactSessionPromotionRequest,
    types::contact::ContactSessionPromotionResponse, types::contact::ContactTokenClaims,
    types::contact::ContactTokenIssueRequest, types::contact::ContactTokenIssueResponse,
    types::contact::ContactVerificationCompleteRequest,
    types::contact::ContactVerificationCompleteResponse,
    types::contact::ContactVerificationStartRequest,
    types::contact::ContactVerificationStartResponse, types::contact::SessionActorRef,
    types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::session::SessionMeta, types::session::SessionStatus,
};
use moa_messaging::ProviderDeliverySink;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_wire::turn::{SessionProgress, SessionProgressRequest, StartTurnRequest};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::handlers::authz_shim::AuthzEnforcer;
use crate::objects::session::SessionClient;
use crate::restate_identity::with_identity_headers;
use crate::services::session_store::inner::{
    change_contact_session_channel_atomic, initialize_contact_session_atomic,
    resolve_agent_context_for_session,
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
#[derive(Clone)]
pub struct ContactsImpl {
    pool: sqlx::PgPool,
    session_store: Arc<moa_session::PostgresSessionStore>,
    config: Arc<MoaConfig>,
    contact_token_issuer: Option<Arc<moa_auth_providers::ContactTokenIssuer>>,
    delivery_sink: ProviderDeliverySink,
    authz: AuthzEnforcer,
}

impl ContactsImpl {
    /// Creates the contact adapter with one process-owned provider delivery sink.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        session_store: Arc<moa_session::PostgresSessionStore>,
        config: Arc<MoaConfig>,
        contact_token_issuer: Option<Arc<moa_auth_providers::ContactTokenIssuer>>,
        delivery_sink: ProviderDeliverySink,
        authz: AuthzEnforcer,
    ) -> Self {
        Self {
            pool,
            session_store,
            config,
            contact_token_issuer,
            delivery_sink,
            authz,
        }
    }

    fn contact_token_issuer(
        &self,
    ) -> Result<Arc<moa_auth_providers::ContactTokenIssuer>, HandlerError> {
        self.contact_token_issuer.clone().ok_or_else(|| {
            TerminalError::new_with_code(503, "contact token signing keys are not configured")
                .into()
        })
    }

    fn verify_contact_token(
        &self,
        token: &str,
        tenant_id: TenantId,
    ) -> Result<ContactTokenClaims, HandlerError> {
        let claims = self
            .contact_token_issuer()?
            .verify(token)
            .map_err(contact_token_handler_error)?;
        if claims.tenant_id != tenant_id {
            return Err(TerminalError::new_with_code(403, "contact token tenant mismatch").into());
        }
        Ok(claims)
    }
}

impl Contacts for ContactsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn issue_token(
        &self,
        ctx: Context<'_>,
        request: Json<ContactTokenIssueRequest>,
    ) -> Result<Json<ContactTokenIssueResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Contacts", "issue_token");
        let request = request.into_inner();
        let identity = self
            .authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;
        let tenant_id = request.tenant_id;
        let token_issuer = self.contact_token_issuer()?;
        let pool = self.pool.clone();
        let contact_point_hash_key_hex = self
            .config
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
        let grant_pool = self.pool.clone();
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Contacts", "start_verification");
        let request = request.into_inner();
        let claims = self.verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "contact:verify:start")
            .map_err(contact_error_handler_error)?;
        require_contact_session_permission(&claims, request.session_id)
            .map_err(contact_error_handler_error)?;
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
        annotate_claim_contact_span(&claims, request.session_id);
        let tenant_id = claims.tenant_id;
        let pool = self.pool.clone();
        let store = self.session_store.clone();
        let config = self.config.clone();
        let ttl_seconds = config.auth.contact_tokens.verification_ttl_seconds;
        let contact_point_hash_key_hex = config
            .auth
            .contact_tokens
            .contact_point_hash_key_hex
            .clone();
        let delivery_sink = self.delivery_sink.clone();
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
                ContactVerifier::new(pool, delivery_sink)
                    .start_verification(ContactVerificationStartCommand {
                        tenant_id,
                        contact_id,
                        contact_point,
                        requested_channel: delivery_channel,
                        ttl_seconds,
                        contact_point_hash_key_hex,
                    })
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Contacts", "complete_verification");
        let request = request.into_inner();
        let claims = self.verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "contact:verify:complete")
            .map_err(contact_error_handler_error)?;
        require_contact_session_permission(&claims, request.session_id)
            .map_err(contact_error_handler_error)?;
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
        annotate_claim_contact_span(&claims, request.session_id);
        let tenant_id = claims.tenant_id;
        let token_issuer = self.contact_token_issuer()?;
        let pool = self.pool.clone();
        let store = self.session_store.clone();
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
        let grant_pool = self.pool.clone();
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Contacts", "init_session");
        let request = request.into_inner();
        let claims = self.verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "agent:session:create")
            .map_err(contact_error_handler_error)?;
        require_contact_agent_permission(&claims, &request.agent)
            .map_err(contact_error_handler_error)?;
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
        annotate_claim_contact_span(&claims, None);
        let tenant_id = claims.tenant_id;
        let pool = self.pool.clone();
        let store = self.session_store.clone();
        let store_backend = self.session_store.clone();
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
        // Journal a replay-stable session id in a side-effect-free step so a
        // handler replay reuses the same identity instead of minting a second
        // complete session. The idempotent creation transaction below keys all
        // product writes on this id.
        let session_id: SessionId = ctx
            .run(|| async move { Ok::<_, HandlerError>(Json::from(SessionId::new())) })
            .name("contacts_allocate_session_id")
            .await?
            .into_inner();
        let meta = SessionMeta {
            id: session_id,
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
        let meta_for_vo = ctx
            .run(|| async move {
                let agent_context =
                    resolve_agent_context_for_session(resolver_pool, &meta, &agent_selection)
                        .await?;
                let mut meta = meta;
                meta.agent_context = Some(agent_context);
                let identity = contact_identity(contact.contact_id, tenant_id);
                let binding = SessionChannelBindingUpdate {
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
                };
                let created_event = Event::SessionCreated {
                    tenant_id,
                    contact_id: Some(contact.contact_id),
                    created_by: Some(SessionActorRef::Contact {
                        id: contact.contact_id,
                    }),
                    model,
                    channel: event_channel,
                };
                // Session row, agent sidecar, authz tuples, initial binding, and
                // the SessionCreated event commit atomically here. The binding id
                // is fresh but only consumed when the session is freshly
                // inserted, so a replay (session already present) never creates a
                // second binding or event.
                initialize_contact_session_atomic(
                    store_backend.as_ref(),
                    &create_pool,
                    meta,
                    identity,
                    SessionChannelBindingId::new(),
                    binding,
                    created_event,
                )
                .await?;
                let meta_for_vo = store
                    .get_session(session_id)
                    .await
                    .map_err(session_store_handler_error)?;
                Ok::<_, HandlerError>(Json::from(meta_for_vo))
            })
            .name("contacts_create_session")
            .await?
            .into_inner();
        crate::restate_identity::replay_safe_request(
            ctx.object_client::<SessionClient>(session_id.to_string())
                .set_meta(Json::from(meta_for_vo)),
        )
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Contacts", "change_session_channel");
        let request = request.into_inner();
        let claims = self.verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "contact:session:channel:update")
            .map_err(contact_error_handler_error)?;
        require_contact_session_permission(&claims, Some(request.session_id))
            .map_err(contact_error_handler_error)?;
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
        annotate_claim_contact_span(&claims, Some(request.session_id));
        let session_id = request.session_id;
        let tenant_id = claims.tenant_id;
        let pool = self.pool.clone();
        let store = self.session_store.clone();
        let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
        let store_backend = self.session_store.clone();
        let change_pool = pool.clone();
        // Journal a replay-stable binding id so the channel-change transaction is
        // idempotent: a replay reuses this id, the binding insert conflicts, and
        // no duplicate binding or SessionChannelChanged event is written.
        let binding_id: SessionChannelBindingId = ctx
            .run(
                || async move { Ok::<_, HandlerError>(Json::from(SessionChannelBindingId::new())) },
            )
            .name("contacts_allocate_channel_binding_id")
            .await?
            .into_inner();

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
                let binding = SessionChannelBindingUpdate {
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
                };
                let changed_event = Event::SessionChannelChanged {
                    from: existing_meta.channel,
                    to: resolved.channel_ref.channel(),
                    contact_id: Some(contact.contact_id),
                    from_binding_id: existing_meta.active_channel_binding_id,
                    to_binding_id: Some(binding_id),
                    changed_by: Some(SessionActorRef::Contact {
                        id: contact.contact_id,
                    }),
                    reason: request.reason,
                };
                change_contact_session_channel_atomic(
                    store_backend.as_ref(),
                    &change_pool,
                    binding_id,
                    binding,
                    changed_event,
                )
                .await?;
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
        crate::restate_identity::replay_safe_request(
            ctx.object_client::<SessionClient>(session_id.to_string())
                .set_meta(Json::from(meta)),
        )
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Contacts", "send_message");
        let request = request.into_inner();
        if let Err(message) = request.validate_admitted_payload() {
            return Err(TerminalError::new_with_code(400, message).into());
        }
        let claims = self.verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "contact:session:message:send")
            .map_err(contact_error_handler_error)?;
        require_contact_session_permission(&claims, Some(request.session_id))
            .map_err(contact_error_handler_error)?;
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
        annotate_claim_contact_span(&claims, Some(request.session_id));
        let session_id = request.session_id;
        let tenant_id = claims.tenant_id;
        let pool = self.pool.clone();
        let store = self.session_store.clone();

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
                .start_turn(Json::from(StartTurnRequest {
                    // The contact's own retry identity, reply target, and stream cursor
                    // pass through untouched: this service authenticates and routes the
                    // message, and the Session VO owns the admission decision.
                    client_message_id: request.client_message_id,
                    reply_to: request.reply_to,
                    stream_cursor: request.stream_cursor,
                    user_message: request.user_message,
                    attachments: request.attachments,
                    model: request.model,
                    contact: None,
                    max_turns: request.max_turns,
                    resource_budget: Default::default(),
                    execution_template: None,
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
            started_turn_id: response.turn_id,
            stream_cursor: response.stream_cursor,
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: contact JWT verification, scope checks, and session ownership validation bind this authorization to one contact session.
    async fn authorize_session(
        &self,
        ctx: Context<'_>,
        request: Json<ContactSessionAuthorizationRequest>,
    ) -> Result<Json<ContactSessionAuthorizationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Contacts", "authorize_session");
        let request = request.into_inner();
        let claims = self.verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "contact:session:message:send")
            .map_err(contact_error_handler_error)?;
        require_contact_session_permission(&claims, Some(request.session_id))
            .map_err(contact_error_handler_error)?;
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
        annotate_claim_contact_span(&claims, Some(request.session_id));
        let session_id = request.session_id;
        let tenant_id = claims.tenant_id;
        let pool = self.pool.clone();
        let store = self.session_store.clone();

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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Contacts", "progress");
        let request = request.into_inner();
        let claims = self.verify_contact_token(&request.contact_token, request.tenant_id)?;
        require_contact_scope(&claims, "contact:session:message:send")
            .map_err(contact_error_handler_error)?;
        require_contact_session_permission(&claims, Some(request.session_id))
            .map_err(contact_error_handler_error)?;
        let contact_id = contact_id_from_claims(&claims).map_err(contact_error_handler_error)?;
        annotate_claim_contact_span(&claims, Some(request.session_id));
        let session_id = request.session_id;
        let tenant_id = claims.tenant_id;
        let pool = self.pool.clone();
        let store = self.session_store.clone();

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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Contacts", "promote_session");
        let request = request.into_inner();
        let claims = self.verify_contact_token(&request.contact_token, request.tenant_id)?;
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
        let pool = self.pool.clone();
        let store = self.session_store.clone();

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
        crate::restate_identity::replay_safe_request(
            ctx.object_client::<SessionClient>(request.session_id.to_string())
                .set_meta(Json::from(meta)),
        )
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
    session_id: moa_core::types::identifiers::SessionId,
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

fn annotate_contact_operation_span(
    contact: &ContactRef,
    session_id: Option<moa_core::types::identifiers::SessionId>,
) {
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
    session_id: Option<moa_core::types::identifiers::SessionId>,
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
    session_id: moa_core::types::identifiers::SessionId,
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
    session_id: moa_core::types::identifiers::SessionId,
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
    session_id: moa_core::types::identifiers::SessionId,
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

fn contact_error_handler_error(error: Error) -> HandlerError {
    match error {
        Error::Terminal { code, message } => TerminalError::new_with_code(code, message).into(),
        Error::SessionStore(MoaError::SessionNotFound(_)) => {
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
