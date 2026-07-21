//! End-to-end coverage for session-authorized public memory reads through Restate.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use moa_authz::fga_subject;
use moa_core::types::security::SensitivityClass;
use moa_core::{
    traits::{Identity, IdentityType, SessionStore as _},
    types::agent::{AgentContext, AgentKnowledgePolicy, AgentPolicySnapshot},
    types::contact::{ContactId, ContactRef, ContactVerificationState, SessionActorRef},
    types::identifiers::{SessionId, TenantId},
    types::memory::{InformationBarrierClearances, InformationBarrierId, RlsContext},
    types::session::SessionMeta,
};
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PostgresGraphStore,
};
use moa_session::testing;
use moa_test_support::OrchestratorTestFixture;
use moa_wire::memory::{
    MemoryIngestDocument, MemoryIngestRequest, MemoryIngestResponse, MemoryRetrieveDebugRequest,
    MemoryRetrieveDebugResponse, MemorySearchRequest, MemorySearchResponse, MemoryShowRequest,
    MemoryShowResponse,
};
use reqwest::RequestBuilder;
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires local Restate, OpenFGA, and superuser-capable Postgres"]
async fn public_memory_reads_use_persisted_session_policy_and_durable_audit_service_e2e()
-> Result<()> {
    // Pins: every public read authorizes the session, derives the complete
    // policy and barrier clearance from the persisted snapshot, rejects legacy
    // caller policy fields, and audits the exact actor before returning data.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .context("create isolated memory service database")?;
    let pool = session_store.pool().clone();
    let fixture = OrchestratorTestFixture::with_script_and_env(
        json!({
            "default": {
                "content": "ok",
                "stop_reason": "end_turn"
            }
        }),
        vec![("MOA_DATABASE_URL".to_string(), database_url.clone())],
    )
    .await
    .context("start shared orchestrator fixture for memory service e2e")?;

    let result = async {
        let tenant_id = TenantId::new();
        let barrier = InformationBarrierId::parse("matter-public-alpha")?;
        let actor_id = Uuid::now_v7();
        let identity = Identity {
            identity_type: IdentityType::Agent,
            id: actor_id,
            tenant_id,
            api_key_id: Some(Uuid::now_v7()),
            acting_on_behalf_of: Some(Uuid::now_v7()),
        };
        let cleared_session = create_session(
            &session_store,
            tenant_id,
            actor_id,
            [barrier.clone()].into_iter().collect(),
            "public-cleared-v1",
        )
        .await?;
        let uncleared_session = create_session(
            &session_store,
            tenant_id,
            actor_id,
            InformationBarrierClearances::new(),
            "public-uncleared-v1",
        )
        .await?;
        grant_session_participant(&fixture, &identity, cleared_session).await?;
        grant_session_participant(&fixture, &identity, uncleared_session).await?;

        let writer = PostgresGraphStore::scoped_for_app_role(
            pool.clone(),
            RlsContext::tenant(tenant_id).with_cleared_barriers(
                [barrier.clone()]
                    .into_iter()
                    .collect::<InformationBarrierClearances>(),
            ),
            Arc::new(moa_crypto::LocalKmsProvider::new()),
        );
        let seed_uid = writer
            .create_node(node(
                tenant_id,
                NodeLabel::Chunk,
                "public memory alpha seed",
                barrier.clone(),
            ))
            .await?;
        let admitted_neighbor_uid = writer
            .create_node(node(
                tenant_id,
                NodeLabel::Chunk,
                "public memory admitted neighbor",
                barrier.clone(),
            ))
            .await?;
        let hidden_neighbor_uid = writer
            .create_node(node(
                tenant_id,
                NodeLabel::Fact,
                "public memory hidden operational fact",
                barrier,
            ))
            .await?;
        for neighbor_uid in [admitted_neighbor_uid, hidden_neighbor_uid] {
            writer
                .create_edge(EdgeWriteIntent {
                    uid: Uuid::now_v7(),
                    label: EdgeLabel::RelatesTo,
                    start_uid: seed_uid,
                    end_uid: neighbor_uid,
                    valid_from: Utc::now(),
                    properties: json!({ "source": "memory-service-e2e" }),
                    storage_partition_id: Some(tenant_id.to_string()),
                    contact_id: None,
                    scope: "tenant".to_string(),
                    actor_id: actor_id.to_string(),
                    actor_kind: "system".to_string(),
                })
                .await?;
        }

        let client = reqwest::Client::new();
        let ingress = fixture.ingress_url.as_str();
        let search_request = MemorySearchRequest {
            session_id: cleared_session,
            query: "public memory alpha seed".to_string(),
            limit: 20,
        };
        let search_idempotency_key = format!("memory-search-{}", Uuid::now_v7());
        let search: MemorySearchResponse = call_memory(
            &client,
            ingress,
            "search",
            &identity,
            &search_request,
            Some(&search_idempotency_key),
        )
        .await?;
        assert_eq!(
            search.hits.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
            vec![seed_uid],
            "the pinned retrieval budget and tenant-knowledge labels must be enforced"
        );
        let replay: MemorySearchResponse = call_memory(
            &client,
            ingress,
            "search",
            &identity,
            &search_request,
            Some(&search_idempotency_key),
        )
        .await?;
        assert_eq!(
            replay, search,
            "Restate replay must return the journaled result"
        );

        let uncleared: MemorySearchResponse = call_memory(
            &client,
            ingress,
            "search",
            &identity,
            &MemorySearchRequest {
                session_id: uncleared_session,
                query: "public memory alpha seed".to_string(),
                limit: 20,
            },
            None,
        )
        .await?;
        assert!(
            uncleared.hits.is_empty(),
            "a participating but uncleared session must not see barriered memory"
        );

        let shown: MemoryShowResponse = call_memory(
            &client,
            ingress,
            "show",
            &identity,
            &MemoryShowRequest {
                session_id: cleared_session,
                uid: seed_uid,
                neighbor_depth: 1,
            },
            None,
        )
        .await?;
        assert_eq!(shown.uid, seed_uid);
        assert_eq!(
            shown
                .neighbors
                .iter()
                .map(|neighbor| neighbor.uid)
                .collect::<Vec<_>>(),
            vec![admitted_neighbor_uid],
            "show must apply pinned admission to every neighbor"
        );
        let hidden_show = send_memory(
            &client,
            ingress,
            "show",
            &identity,
            &MemoryShowRequest {
                session_id: uncleared_session,
                uid: seed_uid,
                neighbor_depth: 0,
            },
            None,
        )
        .await?;
        assert!(
            !hidden_show.status().is_success(),
            "an uncleared session must not resolve a barriered node"
        );

        let debug: MemoryRetrieveDebugResponse = call_memory(
            &client,
            ingress,
            "retrieve_debug",
            &identity,
            &MemoryRetrieveDebugRequest {
                session_id: cleared_session,
                query: "public memory alpha seed".to_string(),
                limit: 20,
                no_flush_wait: true,
            },
            None,
        )
        .await?;
        assert_eq!(
            debug.hits.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
            vec![seed_uid]
        );
        assert_eq!(debug.diagnostics["policy_source"], "pinned_session");

        let forged = send_memory(
            &client,
            ingress,
            "search",
            &identity,
            &json!({
                "session_id": cleared_session,
                "query": "public memory alpha seed",
                "limit": 20,
                "tenant_id": TenantId::new(),
                "cleared_barriers": [],
                "retrieval_operation_id": "forged"
            }),
            None,
        )
        .await?;
        assert!(
            !forged.status().is_success(),
            "legacy caller-owned authorization context must be rejected"
        );

        let events: Vec<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT retrieval_operation_id, event_jcs FROM security_events \
             WHERE tenant_id = $1 AND retrieval_operation_id LIKE 'memory.%:%'",
        )
        .bind(tenant_id.0)
        .fetch_all(&pool)
        .await?;
        assert_eq!(
            events
                .iter()
                .filter(|(operation, _)| operation.starts_with("memory.search:"))
                .count(),
            2,
            "one cleared search replay and one uncleared search produce two logical events"
        );
        assert!(
            events
                .iter()
                .any(|(operation, _)| operation.starts_with("memory.show:"))
        );
        assert!(
            events
                .iter()
                .any(|(operation, _)| operation.starts_with("memory.retrieve_debug:"))
        );
        let cleared_search_event = events
            .iter()
            .map(|(operation, bytes)| {
                serde_json::from_slice::<Value>(bytes).map(|payload| (operation, payload))
            })
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .find(|(operation, payload)| {
                operation.starts_with("memory.search:")
                    && payload["actor"]["session"]["uid"] == format!("session:{cleared_session}")
                    && payload["access"]["node_uids"] == json!([seed_uid.to_string()])
            })
            .map(|(_, payload)| payload)
            .context("find cleared search access event")?;
        assert_eq!(
            cleared_search_event["access"]["api_key_uid"],
            Value::from(format!(
                "api_key:{}",
                identity.api_key_id.expect("test API key")
            ))
        );
        assert_eq!(
            cleared_search_event["access"]["acting_on_behalf_of_uid"],
            Value::from(format!(
                "principal:{}",
                identity.acting_on_behalf_of.expect("test delegation")
            ))
        );

        Ok(())
    }
    .await;

    drop(fixture);
    pool.close().await;
    drop(session_store);
    let cleanup = testing::cleanup_test_schema(&database_url, &schema_name).await;
    result.and(cleanup.map_err(anyhow::Error::from))
}

#[tokio::test]
#[ignore = "requires local Restate, OpenFGA, and superuser-capable Postgres"]
async fn document_ingest_applies_barrier_to_persisted_contact_memory_service_e2e() -> Result<()> {
    // Pins: Memory/ingest_documents carries one typed barrier through Restate into every
    // persisted fact, and public reads expose those facts only to a cleared session.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .context("create isolated document ingestion database")?;
    let pool = session_store.pool().clone();
    let fixture = OrchestratorTestFixture::with_script_and_env(
        json!({
            "default": {
                "content": "ok",
                "stop_reason": "end_turn"
            }
        }),
        vec![("MOA_DATABASE_URL".to_string(), database_url.clone())],
    )
    .await
    .context("start shared orchestrator fixture for document ingestion e2e")?;

    let result = async {
        let tenant_id = TenantId::new();
        let contact_id = ContactId::new();
        let barrier = InformationBarrierId::parse("finance-document-alpha")?;
        let identity = Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::now_v7(),
            tenant_id,
            api_key_id: Some(Uuid::now_v7()),
            acting_on_behalf_of: None,
        };
        fixture
            .grant_tenant_operator_identity(&identity, tenant_id)
            .await?;

        let cleared_session = create_contact_session(
            &session_store,
            tenant_id,
            contact_id,
            identity.id,
            [barrier.clone()].into_iter().collect(),
            "document-cleared-v1",
        )
        .await?;
        let uncleared_session = create_contact_session(
            &session_store,
            tenant_id,
            contact_id,
            identity.id,
            InformationBarrierClearances::new(),
            "document-uncleared-v1",
        )
        .await?;
        grant_session_participant(&fixture, &identity, cleared_session).await?;
        grant_session_participant(&fixture, &identity, uncleared_session).await?;

        let source_name = "finance operating notes";
        let ingest: MemoryIngestResponse = call_memory(
            &reqwest::Client::new(),
            fixture.ingress_url.as_str(),
            "ingest_documents",
            &identity,
            &MemoryIngestRequest {
                tenant_id,
                contact_id: Some(contact_id),
                information_barrier: Some(barrier.clone()),
                documents: vec![MemoryIngestDocument {
                    source_name: source_name.to_string(),
                    content: [
                        "Fact: finance auth service uses JWT access tokens",
                        "Fact: finance billing service owns invoice reconciliation",
                        "Fact: finance incident commander escalates payment outages",
                    ]
                    .join("\n"),
                    source_uri: Some("memory://finance/operating-notes".to_string()),
                    metadata: json!({ "suite": "memory-service-e2e" }),
                }],
            },
            Some(&format!("memory-ingest-{}", Uuid::now_v7())),
        )
        .await?;
        assert_eq!(ingest.results.len(), 1);
        assert_eq!(ingest.results[0].source_name, source_name);
        assert_eq!(
            ingest.results[0].inserted, 3,
            "unexpected ingest report: {ingest:?}"
        );
        assert_eq!(
            ingest.results[0].failed, 0,
            "unexpected ingest report: {ingest:?}"
        );

        let barriered_fact_count = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM moa.node_index \
             WHERE tenant_id = $1 AND contact_id = $2 AND label = 'Fact' AND barrier = $3",
        )
        .bind(tenant_id.0)
        .bind(contact_id.0)
        .bind(barrier.as_str())
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            barriered_fact_count, 3,
            "every fact from the public document request must keep its barrier tag"
        );

        let client = reqwest::Client::new();
        let query = "finance auth service uses JWT access tokens";
        let cleared: MemorySearchResponse = call_memory(
            &client,
            fixture.ingress_url.as_str(),
            "search",
            &identity,
            &MemorySearchRequest {
                session_id: cleared_session,
                query: query.to_string(),
                limit: 10,
            },
            None,
        )
        .await?;
        assert!(
            !cleared.hits.is_empty(),
            "a cleared session must retrieve the barriered document facts"
        );

        let uncleared: MemorySearchResponse = call_memory(
            &client,
            fixture.ingress_url.as_str(),
            "search",
            &identity,
            &MemorySearchRequest {
                session_id: uncleared_session,
                query: query.to_string(),
                limit: 10,
            },
            None,
        )
        .await?;
        assert!(
            uncleared.hits.is_empty(),
            "an uncleared session must not retrieve the barriered document facts"
        );

        Ok(())
    }
    .await;

    drop(fixture);
    pool.close().await;
    drop(session_store);
    let cleanup = testing::cleanup_test_schema(&database_url, &schema_name).await;
    result.and(cleanup.map_err(anyhow::Error::from))
}

async fn grant_session_participant(
    fixture: &OrchestratorTestFixture,
    identity: &Identity,
    session_id: SessionId,
) -> Result<()> {
    let fga = fixture
        .fga_client
        .as_ref()
        .context("shared orchestrator fixture must provide OpenFGA")?;
    fga.apply_raw(json!({
        "authorization_model_id": fga.model_id(),
        "writes": {
            "tuple_keys": [{
                "user": fga_subject(identity),
                "relation": "participant",
                "object": format!("session:{session_id}"),
            }]
        },
    }))
    .await
    .context("grant fixture session participation")?;
    if let (IdentityType::Agent, Some(operator_id)) =
        (identity.identity_type, identity.acting_on_behalf_of)
    {
        fga.apply_raw(json!({
            "authorization_model_id": fga.model_id(),
            "writes": {
                "tuple_keys": [{
                    "user": format!("operator:{operator_id}"),
                    "relation": "can_act_as",
                    "object": format!("agent:{}", identity.id),
                }]
            },
        }))
        .await
        .context("grant fixture agent delegation")?;
    }
    Ok(())
}

async fn create_session(
    store: &moa_session::PostgresSessionStore,
    tenant_id: TenantId,
    actor_id: Uuid,
    clearances: InformationBarrierClearances,
    policy_hash: &str,
) -> Result<SessionId> {
    let mut agent_context = AgentContext::system_default();
    agent_context.policy_hash = policy_hash.to_string();
    agent_context.policy_snapshot = json!(AgentPolicySnapshot {
        knowledge_policy: AgentKnowledgePolicy {
            filters: json!({ "labels": ["Chunk"] }),
            retrieval_budget: Some(1),
            pii_floor: Some("none".to_string()),
            cleared_barriers: clearances,
            ..AgentKnowledgePolicy::default()
        },
        ..AgentPolicySnapshot::default()
    });
    store
        .create_session(SessionMeta {
            id: SessionId::new(),
            tenant_id,
            created_by: Some(SessionActorRef::Identity { id: actor_id }),
            agent_context: Some(agent_context),
            ..SessionMeta::default()
        })
        .await
        .map_err(anyhow::Error::from)
}

async fn create_contact_session(
    store: &moa_session::PostgresSessionStore,
    tenant_id: TenantId,
    contact_id: ContactId,
    actor_id: Uuid,
    clearances: InformationBarrierClearances,
    policy_hash: &str,
) -> Result<SessionId> {
    let mut agent_context = AgentContext::system_default();
    agent_context.policy_hash = policy_hash.to_string();
    agent_context.policy_snapshot = json!(AgentPolicySnapshot {
        knowledge_policy: AgentKnowledgePolicy {
            filters: json!({ "labels": ["Fact"] }),
            retrieval_budget: Some(10),
            pii_floor: Some("none".to_string()),
            cleared_barriers: clearances,
            ..AgentKnowledgePolicy::default()
        },
        ..AgentPolicySnapshot::default()
    });
    store
        .create_session(SessionMeta {
            id: SessionId::new(),
            tenant_id,
            contact: Some(ContactRef {
                contact_id,
                tenant_id,
                state: ContactVerificationState::Verified,
                canonical_contact_id: None,
                linked_contact_ids: Vec::new(),
                scopes: Vec::new(),
                permissions: Value::Null,
                agent_ids: Vec::new(),
                session_ids: Vec::new(),
                verified_contact_point_ids: Vec::new(),
            }),
            created_by: Some(SessionActorRef::Identity { id: actor_id }),
            agent_context: Some(agent_context),
            ..SessionMeta::default()
        })
        .await
        .map_err(anyhow::Error::from)
}

fn node(
    tenant_id: TenantId,
    label: NodeLabel,
    name: &str,
    barrier: InformationBarrierId,
) -> NodeWriteIntent {
    NodeWriteIntent {
        uid: Uuid::now_v7(),
        data_subject_id: tenant_id.0,
        label,
        storage_partition_id: Some(tenant_id.to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        name: name.to_string(),
        properties: json!({ "summary": name }),
        pii_class: SensitivityClass::None,
        confidence: Some(0.95),
        valid_from: Utc::now(),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        embedding_text: None,
        barrier: Some(barrier),
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

async fn call_memory<Req, Resp>(
    client: &reqwest::Client,
    ingress: &str,
    handler: &str,
    identity: &Identity,
    request: &Req,
    idempotency_key: Option<&str>,
) -> Result<Resp>
where
    Req: serde::Serialize + ?Sized,
    Resp: serde::de::DeserializeOwned,
{
    let response =
        send_memory(client, ingress, handler, identity, request, idempotency_key).await?;
    let status = response.status();
    if !status.is_success() {
        bail!(
            "Memory/{handler} failed with {status}: {}",
            response.text().await.unwrap_or_default()
        );
    }
    response.json().await.context("decode memory response")
}

async fn send_memory<Req>(
    client: &reqwest::Client,
    ingress: &str,
    handler: &str,
    identity: &Identity,
    request: &Req,
    idempotency_key: Option<&str>,
) -> Result<reqwest::Response>
where
    Req: serde::Serialize + ?Sized,
{
    let builder = client
        .post(format!(
            "{}/restate/call/Memory/{handler}",
            ingress.trim_end_matches('/')
        ))
        .json(request);
    let builder = with_exact_identity(builder, identity);
    let builder = if let Some(key) = idempotency_key {
        builder.header("idempotency-key", key)
    } else {
        builder
    };
    builder.send().await.context("call memory service")
}

fn with_exact_identity(request: RequestBuilder, identity: &Identity) -> RequestBuilder {
    let mut request = request
        .header("x-moa-identity-type", identity.identity_type.as_str())
        .header("x-moa-identity-id", identity.id.to_string())
        .header("x-moa-tenant-id", identity.tenant_id.to_string());
    if let Some(id) = identity.api_key_id {
        request = request.header("x-moa-api-key-id", id.to_string());
    }
    if let Some(id) = identity.acting_on_behalf_of {
        request = request.header("x-moa-acting-on-behalf-of", id.to_string());
    }
    request
}
