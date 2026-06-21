//! Restate service for agent-facing contact identity operations.

use chrono::{DateTime, Duration, Utc};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::Identity;
use moa_core::{
    ContactDeliveryChannel, ContactId, ContactPointId, ContactPointInput, ContactPointKind,
    ContactPointRef, ContactRef, ContactSessionInitRequest, ContactSessionInitResponse,
    ContactSessionPromotionRequest, ContactSessionPromotionResponse, ContactTokenClaims,
    ContactTokenIssueRequest, ContactTokenIssueResponse, ContactVerificationChallengeId,
    ContactVerificationCompleteRequest, ContactVerificationCompleteResponse,
    ContactVerificationStartRequest, ContactVerificationStartResponse, ContactVerificationState,
    ModelId, Platform, SessionActorRef, SessionMeta, SessionStatus, WorkspaceId,
};
use moa_core::{MoaError, SessionStore};
use moa_messaging::{DeliveryMessage, DeliverySink, ProviderDeliverySink};
use rand::Rng;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::objects::session::SessionClient;

const LOW_ASSURANCE_SCOPES: &[&str] = &[
    "agent:session:create",
    "contact:verify:start",
    "contact:verify:complete",
    "memory:session:read",
    "memory:session:write",
];
const VERIFIED_SCOPES: &[&str] = &[
    "agent:session:create",
    "contact:verify:start",
    "contact:verify:complete",
    "contact:self:update",
    "contact:session:promote",
    "memory:session:read",
    "memory:session:write",
    "memory:self:read",
    "memory:self:write",
];
const MAX_LINKED_CONTACT_IDS: i64 = 8;
const MAX_VERIFICATION_ATTEMPTS: i32 = 5;

/// Restate surface for contact identity and contact-scoped sessions.
#[restate_sdk::service]
#[name = "Contacts"]
pub trait Contacts {
    /// Issues a low-assurance contact token for a workspace contact.
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
        authorize_workspace_admin(&identity, &request.workspace_id).await?;
        let tenant_id = identity.tenant_id;
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
    // SAFETY: contact JWT verification and scope checks bound this operation to one contact and workspace.
    async fn start_verification(
        &self,
        ctx: Context<'_>,
        request: Json<ContactVerificationStartRequest>,
    ) -> Result<Json<ContactVerificationStartResponse>, HandlerError> {
        annotate_restate_handler_span("Contacts", "start_verification");
        let request = request.into_inner();
        let claims = verify_contact_token(&request.contact_token, &request.workspace_id)?;
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
                        &request.workspace_id,
                        contact_id,
                    )
                    .await?;
                }
                start_contact_verification(
                    pool,
                    ContactVerificationStartCommand {
                        tenant_id,
                        workspace_id: request.workspace_id,
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
    // SAFETY: contact JWT verification and one-time challenge verification bind promotion to the contact point.
    async fn complete_verification(
        &self,
        ctx: Context<'_>,
        request: Json<ContactVerificationCompleteRequest>,
    ) -> Result<Json<ContactVerificationCompleteResponse>, HandlerError> {
        annotate_restate_handler_span("Contacts", "complete_verification");
        let request = request.into_inner();
        let claims = verify_contact_token(&request.contact_token, &request.workspace_id)?;
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
                        &request.workspace_id,
                        contact_id,
                    )
                    .await?;
                }
                complete_contact_verification(
                    pool,
                    tenant_id,
                    request.workspace_id,
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
    // SAFETY: contact JWT verification and scope checks bound this session creation to one contact and workspace.
    async fn init_session(
        &self,
        ctx: Context<'_>,
        request: Json<ContactSessionInitRequest>,
    ) -> Result<Json<ContactSessionInitResponse>, HandlerError> {
        annotate_restate_handler_span("Contacts", "init_session");
        let request = request.into_inner();
        let claims = verify_contact_token(&request.contact_token, &request.workspace_id)?;
        require_contact_scope(&claims, "agent:session:create")?;
        let contact_id = contact_id_from_claims(&claims)?;
        annotate_claim_contact_span(&claims, None);
        let tenant_id = claims.tenant_id;
        let pool = OrchestratorCtx::current_graph_pool();
        let store = OrchestratorCtx::current_session_store();
        let workspace_id = request.workspace_id.clone();

        let contact = ctx
            .run(|| async move {
                ensure_contact_token_grant_active(&pool, &claims, contact_id).await?;
                load_contact_ref(pool, &workspace_id, tenant_id, contact_id)
                    .await
                    .map(Json::from)
            })
            .name("contacts_load_session_contact")
            .await?
            .into_inner();
        let meta = SessionMeta {
            workspace_id: request.workspace_id.clone(),
            user_id: contact.contact_id.as_user_id(),
            title: request.title,
            status: SessionStatus::Created,
            platform: Platform::Api,
            platform_channel: request.platform_channel,
            model: ModelId::new(request.model),
            contact: Some(contact.clone()),
            created_by: Some(SessionActorRef::Contact {
                id: contact.contact_id,
            }),
            ..SessionMeta::default()
        };
        let meta_for_vo = meta.clone();
        let session_id = ctx
            .run(|| async move {
                store
                    .create_session(meta)
                    .await
                    .map_err(session_store_handler_error)
                    .map(Json::from)
            })
            .name("contacts_create_session")
            .await?
            .into_inner();
        ctx.object_client::<SessionClient>(session_id.to_string())
            .set_meta(Json::from(meta_for_vo))
            .call()
            .await?;
        annotate_contact_operation_span(&contact, Some(session_id));

        Ok(Json::from(ContactSessionInitResponse {
            session_id,
            contact,
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
        let claims = verify_contact_token(&request.contact_token, &request.workspace_id)?;
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
                let contact =
                    load_contact_ref(pool, &request.workspace_id, tenant_id, contact_id).await?;
                let meta = store
                    .get_session(request.session_id)
                    .await
                    .map_err(session_store_handler_error)?;
                if meta.workspace_id != request.workspace_id {
                    return Err(
                        TerminalError::new_with_code(403, "session workspace mismatch").into(),
                    );
                }
                let promoted_from = promoted_from_contact(&meta, &contact)?;
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
        meta.user_id = contact.contact_id.as_user_id();
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

trait ContactScopesExt {
    fn with_scopes(self, scopes: Vec<String>) -> Self;
}

impl ContactScopesExt for ContactRef {
    fn with_scopes(mut self, scopes: Vec<String>) -> Self {
        self.scopes = scopes;
        self
    }
}

async fn issue_contact(
    pool: sqlx::PgPool,
    tenant_id: Uuid,
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
        INSERT INTO contacts (id, tenant_id, workspace_id, state, display_name, profile, metadata)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id)
    .bind(request.workspace_id.as_str())
    .bind(state.as_str())
    .bind(request.display_name.as_deref())
    .bind(&request.profile)
    .bind(&request.metadata)
    .execute(&mut *transaction)
    .await
    .map_err(|error| db_handler_error("insert contact", error))?;

    let mut contact_points = Vec::with_capacity(request.contact_points.len());
    for point in request.contact_points {
        let contact_point = insert_contact_point(
            &mut transaction,
            tenant_id,
            &request.workspace_id,
            contact_id,
            point,
            false,
        )
        .await?;
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
            workspace_id: request.workspace_id,
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
    tenant_id: Uuid,
    workspace_id: WorkspaceId,
    contact_id: ContactId,
    contact_point: ContactPointInput,
    requested_channel: Option<ContactDeliveryChannel>,
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
    ensure_contact_in_workspace(
        &mut transaction,
        &command.workspace_id,
        command.tenant_id,
        command.contact_id,
    )
    .await?;
    let contact_point = insert_contact_point(
        &mut transaction,
        command.tenant_id,
        &command.workspace_id,
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
          AND workspace_id = $4
          AND consumed_at IS NULL
        "#,
    )
    .bind(command.contact_id.0)
    .bind(contact_point.id.0)
    .bind(command.tenant_id)
    .bind(command.workspace_id.as_str())
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
    .bind(command.tenant_id)
    .bind(command.workspace_id.as_str())
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
        command.workspace_id.as_str(),
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
        command.tenant_id,
        command.workspace_id.clone(),
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
    channel: ContactDeliveryChannel,
    destination: String,
}

fn contact_point_delivery(
    point: &ContactPointInput,
    requested_channel: Option<ContactDeliveryChannel>,
) -> Result<ContactPointDelivery, HandlerError> {
    let channel = match point.kind {
        ContactPointKind::Email => ContactDeliveryChannel::Email,
        ContactPointKind::Phone => ContactDeliveryChannel::Sms,
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
    tenant_id: Uuid,
    workspace_id: WorkspaceId,
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
        WHERE c.id = $1 AND c.contact_id = $2 AND c.tenant_id = $3 AND c.workspace_id = $4
        FOR UPDATE
        "#,
    )
    .bind(challenge_id.0)
    .bind(contact_id.0)
    .bind(tenant_id)
    .bind(workspace_id.as_str())
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
    let normalized_hash = challenge
        .try_get::<String, _>("normalized_hash")
        .map_err(|error| db_handler_error("read contact point hash", error))?;
    let canonical_id = existing_verified_contact(
        &mut transaction,
        tenant_id,
        &workspace_id,
        &kind,
        &normalized_hash,
        contact_id,
    )
    .await?;

    if let Some(canonical_id) = canonical_id {
        sqlx::query(
            r#"
            UPDATE contacts
            SET state = 'merged', canonical_contact_id = $1, merged_at = NOW(), updated_at = NOW()
            WHERE id = $2 AND tenant_id = $3 AND workspace_id = $4
            "#,
        )
        .bind(canonical_id.0)
        .bind(contact_id.0)
        .bind(tenant_id)
        .bind(workspace_id.as_str())
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
            "UPDATE contacts SET state = 'verified', updated_at = NOW() WHERE id = $1 AND tenant_id = $2 AND workspace_id = $3",
        )
        .bind(contact_id.0)
        .bind(tenant_id)
        .bind(workspace_id.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(|error| db_handler_error("mark contact verified", error))?;
    }

    sqlx::query(
        r#"
        UPDATE contact_token_grants
        SET revoked_at = NOW()
        WHERE contact_id = $1
          AND tenant_id = $2
          AND workspace_id = $3
          AND revoked_at IS NULL
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id)
    .bind(workspace_id.as_str())
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

    load_contact_ref(
        pool,
        &workspace_id,
        tenant_id,
        canonical_id.unwrap_or(contact_id),
    )
    .await
}

async fn load_contact_ref(
    pool: sqlx::PgPool,
    workspace_id: &WorkspaceId,
    tenant_id: Uuid,
    contact_id: ContactId,
) -> Result<ContactRef, HandlerError> {
    let row = sqlx::query(
        r#"
        SELECT id, tenant_id, state, canonical_contact_id
        FROM contacts
        WHERE id = $1 AND tenant_id = $2 AND workspace_id = $3
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id)
    .bind(workspace_id.as_str())
    .fetch_optional(&pool)
    .await
    .map_err(|error| db_handler_error("load contact", error))?
    .ok_or_else(|| TerminalError::new_with_code(404, "contact not found"))?;
    let state = row
        .try_get::<String, _>("state")
        .map_err(|error| db_handler_error("read contact state", error))?;
    let links = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id
        FROM contacts
        WHERE canonical_contact_id = $1
          AND tenant_id = $2
          AND workspace_id = $3
        ORDER BY merged_at NULLS LAST, updated_at DESC, id
        LIMIT $4
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id)
    .bind(workspace_id.as_str())
    .bind(MAX_LINKED_CONTACT_IDS)
    .fetch_all(&pool)
    .await
    .map_err(|error| db_handler_error("load linked contacts", error))?;
    Ok(ContactRef {
        contact_id,
        tenant_id,
        workspace_id: workspace_id.clone(),
        state: parse_contact_state(&state)?,
        canonical_contact_id: row
            .try_get::<Option<Uuid>, _>("canonical_contact_id")
            .map_err(|error| db_handler_error("read canonical contact id", error))?
            .map(ContactId),
        linked_contact_ids: links.into_iter().map(ContactId).collect(),
        scopes: Vec::new(),
        permissions: serde_json::Value::Null,
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    })
}

async fn insert_contact_point(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    workspace_id: &WorkspaceId,
    contact_id: ContactId,
    point: ContactPointInput,
    verified: bool,
) -> Result<ContactPointRef, HandlerError> {
    let normalized = normalize_contact_point(point.kind, &point.value)?;
    let normalized_hash = hash_contact_point(workspace_id, point.kind, &normalized)?;
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
    .bind(tenant_id)
    .bind(workspace_id.as_str())
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

async fn ensure_contact_in_workspace(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: &WorkspaceId,
    tenant_id: Uuid,
    contact_id: ContactId,
) -> Result<(), HandlerError> {
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM contacts WHERE id = $1 AND tenant_id = $2 AND workspace_id = $3)",
    )
    .bind(contact_id.0)
    .bind(tenant_id)
    .bind(workspace_id.as_str())
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
    workspace_id: &WorkspaceId,
    contact_id: ContactId,
) -> Result<SessionMeta, HandlerError> {
    let meta = store
        .get_session(session_id)
        .await
        .map_err(session_store_handler_error)?;
    if &meta.workspace_id != workspace_id {
        return Err(TerminalError::new_with_code(403, "session workspace mismatch").into());
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
        workspace_id = %workspace_id,
        "validated contact session binding"
    );
    Ok(meta)
}

fn promoted_from_contact(
    meta: &SessionMeta,
    contact: &ContactRef,
) -> Result<Option<ContactId>, HandlerError> {
    let Some(current) = meta.contact.as_ref() else {
        return Err(TerminalError::new_with_code(403, "session has no contact binding").into());
    };
    if current.tenant_id != contact.tenant_id || current.workspace_id != contact.workspace_id {
        return Err(TerminalError::new_with_code(403, "session contact boundary mismatch").into());
    }
    if current.contact_id == contact.contact_id {
        return Ok(None);
    }
    if contact.linked_contact_ids.contains(&current.contact_id) {
        return Ok(Some(current.contact_id));
    }
    Err(
        TerminalError::new_with_code(403, "session contact is not linked to verified contact")
            .into(),
    )
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
    .bind(claims.tenant_id)
    .bind(claims.workspace_id.as_str())
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
              AND workspace_id = $3
              AND contact_id = $4
              AND state = $5
              AND revoked_at IS NULL
              AND expires_at > NOW()
        )
        "#,
    )
    .bind(&claims.jti)
    .bind(claims.tenant_id)
    .bind(claims.workspace_id.as_str())
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
    tenant_id: Uuid,
    workspace_id: &WorkspaceId,
    kind: &str,
    normalized_hash: &str,
    excluded_contact_id: ContactId,
) -> Result<Option<ContactId>, HandlerError> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT contact_id
        FROM contact_points
        WHERE tenant_id = $1
          AND workspace_id = $2
          AND kind = $3
          AND normalized_hash = $4
          AND verified = TRUE
          AND contact_id <> $5
        LIMIT 1
        "#,
    )
    .bind(tenant_id)
    .bind(workspace_id.as_str())
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
    span.set_attribute("moa.workspace.id", contact.workspace_id.to_string());
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
    span.set_attribute("moa.workspace.id", claims.workspace_id.to_string());
    span.set_attribute("moa.contact.id", claims.sub.clone());
    span.set_attribute("moa.contact.state", claims.state.as_str().to_string());
    if let Some(session_id) = session_id {
        span.set_attribute("moa.session.id", session_id.to_string());
    }
}

async fn authorize_workspace_admin(
    identity: &Identity,
    workspace_id: &WorkspaceId,
) -> Result<(), HandlerError> {
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        identity,
        ObjectType::Workspace,
        workspace_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)
}

fn verify_contact_token(
    token: &str,
    workspace_id: &WorkspaceId,
) -> Result<ContactTokenClaims, HandlerError> {
    let claims = contact_token_issuer()?
        .verify(token)
        .map_err(contact_token_handler_error)?;
    if &claims.workspace_id != workspace_id {
        return Err(TerminalError::new_with_code(403, "contact token workspace mismatch").into());
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
    workspace_id: &WorkspaceId,
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
        format!("{workspace_id}:{}:{normalized}", kind.as_str()).as_bytes(),
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
        ContactDeliveryChannel, ContactId, ContactPointInput, ContactPointKind, ContactRef,
        ContactVerificationState, SessionMeta, WorkspaceId,
    };

    use super::{contact_point_delivery, promoted_from_contact};

    #[test]
    fn promoted_from_contact_allows_linked_session_contact() {
        // Pins: verified contact promotion can carry forward a linked anonymous contact's memory.
        let tenant_id = uuid::Uuid::now_v7();
        let workspace_id = WorkspaceId::new("workspace");
        let linked_contact_id = ContactId::new();
        let verified_contact_id = ContactId::new();
        let linked = contact(
            tenant_id,
            workspace_id.clone(),
            linked_contact_id,
            Vec::new(),
        );
        let verified = contact(
            tenant_id,
            workspace_id.clone(),
            verified_contact_id,
            vec![linked_contact_id],
        );
        let meta = session_meta(linked);

        let promoted_from =
            promoted_from_contact(&meta, &verified).expect("linked contact should promote");

        assert_eq!(promoted_from, Some(linked_contact_id));
    }

    #[test]
    fn promoted_from_contact_rejects_unlinked_session_contact() {
        // Pins: verified contact tokens cannot attach themselves to unrelated workspace sessions.
        let tenant_id = uuid::Uuid::now_v7();
        let workspace_id = WorkspaceId::new("workspace");
        let unrelated_contact_id = ContactId::new();
        let verified_contact_id = ContactId::new();
        let unrelated = contact(
            tenant_id,
            workspace_id.clone(),
            unrelated_contact_id,
            Vec::new(),
        );
        let verified = contact(
            tenant_id,
            workspace_id.clone(),
            verified_contact_id,
            Vec::new(),
        );
        let meta = session_meta(unrelated);

        let error = promoted_from_contact(&meta, &verified)
            .expect_err("unlinked contact should not promote session");

        assert!(
            format!("{error:?}").contains("session contact is not linked to verified contact"),
            "unexpected error: {error:?}"
        );
    }

    fn session_meta(contact: ContactRef) -> SessionMeta {
        SessionMeta {
            workspace_id: contact.workspace_id.clone(),
            user_id: contact.contact_id.as_user_id(),
            contact: Some(contact),
            ..SessionMeta::default()
        }
    }

    fn contact(
        tenant_id: uuid::Uuid,
        workspace_id: WorkspaceId,
        contact_id: ContactId,
        linked_contact_ids: Vec<ContactId>,
    ) -> ContactRef {
        ContactRef {
            contact_id,
            tenant_id,
            workspace_id,
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
        assert_eq!(email.channel, ContactDeliveryChannel::Email);
        assert_eq!(email.destination, "user@example.com");

        let phone = contact_point_delivery(
            &ContactPointInput {
                kind: ContactPointKind::Phone,
                value: "(500) 555-0006".to_string(),
                display_value: None,
            },
            Some(ContactDeliveryChannel::Sms),
        )
        .expect("phone contact point should support SMS delivery");
        assert_eq!(phone.channel, ContactDeliveryChannel::Sms);
        assert_eq!(phone.destination, "+5005550006");

        let mismatch = contact_point_delivery(
            &ContactPointInput {
                kind: ContactPointKind::Email,
                value: "user@example.com".to_string(),
                display_value: None,
            },
            Some(ContactDeliveryChannel::Sms),
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
}
