//! Shared fixtures for the tenant Knowledge service integration-test modules.

mod connections;
mod ingestion;
mod inspection;
mod link_claim;
mod trace;
mod webhook;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use moa_connectors::{
    domain::{
        ConnectionGeneration, ConnectionHealth, ConnectionStatus as ParentConnectionStatus,
        ConnectorConnection, ManagedParentClaim, ManagedParentDefinition,
        ManagedParentDeleteOutcome,
    },
    repository::{
        ConnectionLifecycleRepository, ManagedParentRepository, PostgresConnectionRepository,
    },
    service::{
        ConnectionCredentialSlot, ConnectionCredentialSlotReadiness, ConnectorService,
        CredentialGenerationFenceRequest, CredentialSlotVerifier, ManagedParentActivationRequest,
        ManagedParentClaimRequest, ManagedParentDeleteRequest,
    },
};
use moa_core::types::credentials::{
    CredentialIdentity, CredentialKind, CredentialPrincipal, CredentialRef, CredentialSlotName,
    CredentialStagingToken, RedactedSecret,
};
use moa_core::types::memory::{InformationBarrierId, RlsContext};
use moa_core::types::security::SensitivityClass;
use moa_core::{
    traits::{EmbeddingProvider, Identity, IdentityType},
    types::contact::ContactId,
    types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
    types::identifiers::UserId,
    types::identifiers::{ConnectorConnectionId, SessionId},
};
use moa_db::ScopedConn;
use moa_knowledge::{
    Error as KnowledgeError,
    chunking::ChunkingConfig,
    contact_groups::derive_contact_groups_from_object_with_resolved_members,
    domain::{
        ApplySourceSelectionRequest, ContactGroup, ContactGroupMembership, ContactGroupTarget,
        CreateLinkTokenRequest, DocumentElement, DocumentElementKind, DocumentVersion,
        ElementLayout, ExchangePublicTokenRequest, InitialSyncStarted, KnowledgeBlock,
        KnowledgeChunk, KnowledgeConnection, KnowledgeConnectionDisconnectProgress,
        KnowledgeConnectionProjection, KnowledgeCredentialOwnership,
        KnowledgeDisconnectReservation, KnowledgeDisconnectState, KnowledgeDisconnectTransition,
        KnowledgeIngestionStep, KnowledgeObject, KnowledgeObjectInspection,
        KnowledgeObjectProjection, KnowledgeProviderEventRecord, KnowledgeSyncCounters,
        KnowledgeSyncRun, LinkClaim, LinkClaimReservation, LinkClaimState, LinkClaimTransition,
        LinkToken, LinkedAccount, ListChangedRecordsRequest, NewKnowledgeConnectionDisconnect,
        NewLinkClaim, ObjectStatus, ParseInput, ParsedDocument, ProviderIntegration,
        ProviderRecord, ProviderRecordAcl, RecordPage, RemoteRevokeRequest,
        StartInitialSyncRequest, SyncRunStatus, TriggerSyncRequest, TriggeredSync, WebhookEvent,
    },
    ingestion::{
        KnowledgeIngestionPipeline, KnowledgeIngestionPipelineConfig, MemoryKnowledgeGraphWriter,
        PageIngestionReport,
    },
    parser::DocumentParser,
    providers::LinkedIntegrationProvider,
    repository::{
        DocumentVersionIngestionClaim, KnowledgeDiscoveryStore, PostgresKnowledgeRepository,
        ProviderAccountConnectionLookup, SyncRunClaim, acl::KnowledgeAclRepository,
        connection::KnowledgeConnectionRepository, contact_group::KnowledgeContactGroupRepository,
        document::KnowledgeIngestionRepository, event::KnowledgeEventRepository,
        sync::KnowledgeSyncRepository,
    },
};
use moa_lineage_core::{
    BackendIntrospection, FusedHit, GraphPath, LineageEvent, RecordKind, RerankHit,
    RetrievalLineage, RetrievalSelectedHit, RetrievalStage, StageTimings, TurnId, VecHit,
};
use moa_memory_graph::{GraphStore, NodeLabel, NodeWriteIntent, PostgresGraphStore};
use moa_memory_types::MemoryScope;
use moa_memory_vector::{PgvectorStore, VECTOR_DIMENSION};
use moa_orchestrator::services::knowledge::ingest::KnowledgeIngestionRunner;
use moa_orchestrator::services::knowledge::webhook_verifier::{
    KnowledgeWebhookVerifier, ParserWebhookVerifier,
};
use moa_orchestrator::services::knowledge::{
    KnowledgeCaller, KnowledgeConnectorConnections, KnowledgeCredentialStore,
    KnowledgeRepositoryCapabilities, KnowledgeService, KnowledgeServiceError,
    StagedKnowledgeCredential, StaticKnowledgeProviders,
};
use moa_orchestrator::workflows::knowledge_sync_ingestion::{
    KnowledgeSyncIngestionRequest, KnowledgeSyncIngestionSteps, KnowledgeSyncPageApplication,
    KnowledgeSyncPreparedRun, KnowledgeSyncProviderPage, run_knowledge_sync_ingestion_workflow,
};
use moa_wire::knowledge::{
    KnowledgeConnectionListRequest, KnowledgeDisconnectConnectionRequest,
    KnowledgeExchangeTokenRequest, KnowledgeIntegrationListRequest, KnowledgeObjectInspectRequest,
    KnowledgeObjectListRequest, KnowledgeProviderWebhookRequest, KnowledgeQueryTraceRequest,
    KnowledgeSyncEventsRequest, KnowledgeSyncRequest, KnowledgeSyncStatusRequest,
    KnowledgeUpdateConnectionSourceSelectionRequest,
};
use reqwest::header::HeaderMap;
use restate_sdk::prelude::{HandlerError, TerminalError};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio_util::bytes::Bytes;
use uuid::Uuid;

const PROVIDER: &str = "merge";
const CONNECTOR: &str = "drive";
const SECRET_TOKEN: &str = "provider-secret-token-123";
const SECRET_BEARER: &str = "Bearer provider-secret-token-456";
const RAW_DOCUMENT_TAIL: &str = "RAW_FULL_DOCUMENT_TAIL_SHOULD_NOT_APPEAR";

include!("support/service.rs");
include!("support/sync.rs");
include!("support/ingestion.rs");
include!("support/webhook.rs");
include!("support/provider.rs");
include!("support/connector.rs");
include!("support/credential.rs");
include!("support/repository.rs");
