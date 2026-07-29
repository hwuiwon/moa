//! OCSF v1.3 event class shapes emitted by MOA.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// OCSF schema version emitted by this crate.
pub const SCHEMA_VERSION: &str = "1.3.0";

/// Common OCSF user object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// Stable actor or target UID, such as `user:<uuid>`.
    pub uid: String,
    /// Optional display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional email address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_addr: Option<String>,
    /// OCSF user type id.
    #[serde(rename = "type_id")]
    pub type_id: i32,
}

/// Common OCSF actor object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    /// Actor user or principal.
    pub user: User,
    /// Optional session context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<Session>,
}

/// Common OCSF session object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Session UID.
    pub uid: String,
    /// Optional creation time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_time: Option<DateTime<Utc>>,
}

/// Common OCSF resource object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    /// Stable resource UID, such as `api_key:<uuid>`.
    pub uid: String,
    /// Optional resource name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Resource type.
    #[serde(rename = "type")]
    pub resource_type: String,
}

/// Common OCSF metadata object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    /// OCSF schema version.
    pub version: String,
    /// Product metadata.
    pub product: Product,
}

/// OCSF product metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Product {
    /// Product name.
    pub name: String,
    /// Product vendor.
    pub vendor_name: String,
    /// Product version.
    pub version: String,
}

/// OCSF Network Endpoint object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkEndpoint {
    /// Source IP.
    pub ip: String,
    /// Source port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<i32>,
}

/// OCSF Authentication event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthenticationEvent {
    /// OCSF class UID.
    pub class_uid: i32,
    /// OCSF class name.
    pub class_name: String,
    /// OCSF category UID.
    pub category_uid: i32,
    /// OCSF category name.
    pub category_name: String,
    /// OCSF type UID.
    pub type_uid: i64,
    /// Activity ID.
    pub activity_id: i32,
    /// Activity name.
    pub activity_name: String,
    /// Severity ID.
    pub severity_id: i32,
    /// Severity label.
    pub severity: String,
    /// Status ID.
    pub status_id: i32,
    /// Status label.
    pub status: String,
    /// Event occurrence time.
    pub time: DateTime<Utc>,
    /// OCSF metadata.
    pub metadata: Metadata,
    /// Actor.
    pub actor: Actor,
    /// Authentication protocol.
    pub auth_protocol: String,
    /// Optional source endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src_endpoint: Option<NetworkEndpoint>,
}

/// OCSF Authorize Session event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizationEvent {
    /// OCSF class UID.
    pub class_uid: i32,
    /// OCSF class name.
    pub class_name: String,
    /// OCSF category UID.
    pub category_uid: i32,
    /// OCSF category name.
    pub category_name: String,
    /// OCSF type UID.
    pub type_uid: i64,
    /// Activity ID.
    pub activity_id: i32,
    /// Activity name.
    pub activity_name: String,
    /// Severity ID.
    pub severity_id: i32,
    /// Severity label.
    pub severity: String,
    /// Status ID.
    pub status_id: i32,
    /// Status label.
    pub status: String,
    /// Event occurrence time.
    pub time: DateTime<Utc>,
    /// OCSF metadata.
    pub metadata: Metadata,
    /// Actor.
    pub actor: Actor,
    /// Resource being authorized.
    pub resource: Resource,
    /// Checked or changed privileges.
    pub privileges: Vec<String>,
}

/// OCSF Account Change event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountChangeEvent {
    /// OCSF class UID.
    pub class_uid: i32,
    /// OCSF class name.
    pub class_name: String,
    /// OCSF category UID.
    pub category_uid: i32,
    /// OCSF category name.
    pub category_name: String,
    /// OCSF type UID.
    pub type_uid: i64,
    /// Activity ID.
    pub activity_id: i32,
    /// Activity name.
    pub activity_name: String,
    /// Severity ID.
    pub severity_id: i32,
    /// Severity label.
    pub severity: String,
    /// Event occurrence time.
    pub time: DateTime<Utc>,
    /// OCSF metadata.
    pub metadata: Metadata,
    /// Actor.
    pub actor: Actor,
    /// Account that changed.
    pub user: User,
}

/// OCSF Datastore Activity event: one memory-retrieval data access.
///
/// Emitted once per retrieval operation as a summary — never once per node — so a
/// compliance auditor can answer "who accessed what memory data, and when."
/// Carries no node content or names; the accessed collection is identified only
/// by its scope in [`Resource::uid`], and [`DataAccess`] holds the queryable
/// count, tier, and turn linkage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataAccessEvent {
    /// OCSF class UID.
    pub class_uid: i32,
    /// OCSF class name.
    pub class_name: String,
    /// OCSF category UID.
    pub category_uid: i32,
    /// OCSF category name.
    pub category_name: String,
    /// OCSF type UID.
    pub type_uid: i64,
    /// Activity ID.
    pub activity_id: i32,
    /// Activity name.
    pub activity_name: String,
    /// Severity ID.
    pub severity_id: i32,
    /// Severity label.
    pub severity: String,
    /// Event occurrence time.
    pub time: DateTime<Utc>,
    /// OCSF metadata.
    pub metadata: Metadata,
    /// Accessing principal and session.
    pub actor: Actor,
    /// Accessed memory collection (scope). Never carries node content or names.
    pub resource: Resource,
    /// MOA memory data-access transparency detail.
    pub access: DataAccess,
}

/// MOA memory data-access detail attached to a [`DataAccessEvent`].
///
/// Records the queryable "what scope + how many + which turn" of one retrieval so
/// the access record stays answerable without reading any node content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataAccess {
    /// Replay-stable identity of this logical retrieval operation.
    pub retrieval_operation_id: String,
    /// Exact graph node UIDs returned by this retrieval, sorted and deduplicated.
    pub node_uids: Vec<String>,
    /// Memory scope tier read: `tenant` or `contact`.
    pub scope_tier: String,
    /// Storage partition (tenant boundary) the scoped read executed against.
    pub storage_partition: String,
    /// Source tiers touched by the retrieval, e.g. `tenant_knowledge`, `user_memory`.
    pub source_tiers: Vec<String>,
    /// Number of memory records the retrieval returned (summary count, not per node).
    pub records_returned: u32,
    /// Turn that triggered the retrieval, linking to retrieval lineage. Absent when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_uid: Option<String>,
    /// Agent principal that performed the retrieval, when a configured agent is pinned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_uid: Option<String>,
    /// API key that authenticated the access, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_uid: Option<String>,
    /// Principal on whose behalf the actor ran, when delegated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acting_on_behalf_of_uid: Option<String>,
}

/// OCSF Detection Finding (class 2004).
///
/// MOA emits exactly one of these per prompt-injection circuit transition. It is
/// deliberately content-free: `finding_info` carries the replay-stable transition
/// key, a fixed title and description, and closed-vocabulary identifiers, so a
/// finding is safe to ship to an external SIEM without leaking the attacker's
/// bytes or the tool output that carried them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionFindingEvent {
    /// OCSF class UID.
    pub class_uid: i32,
    /// OCSF class name.
    pub class_name: String,
    /// OCSF category UID.
    pub category_uid: i32,
    /// OCSF category name.
    pub category_name: String,
    /// OCSF type UID.
    pub type_uid: i64,
    /// Activity ID.
    pub activity_id: i32,
    /// Activity name.
    pub activity_name: String,
    /// Severity ID derived deterministically from the stage reached.
    pub severity_id: i32,
    /// Severity label.
    pub severity: String,
    /// Event occurrence time, supplied by the owner rather than read here.
    pub time: DateTime<Utc>,
    /// OCSF metadata.
    pub metadata: Metadata,
    /// Session the transition belongs to.
    pub session: Session,
    /// Capability whose circuit advanced, as the finding's target resource.
    pub resource: Resource,
    /// Finding identity and fixed description.
    pub finding_info: FindingInfo,
    /// MOA circuit detail, entirely closed vocabulary.
    pub circuit: PromptInjectionCircuit,
}

/// OCSF `finding_info` object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingInfo {
    /// Replay-stable transition key; the finding's stable identity.
    pub uid: String,
    /// Fixed safe title.
    pub title: String,
    /// Fixed safe description.
    pub desc: String,
    /// Analytic that produced the finding (the detector policy revision).
    pub analytic: String,
}

/// MOA prompt-injection circuit detail attached to a [`DetectionFindingEvent`].
///
/// Every field is drawn from a closed vocabulary or is an identifier MOA minted,
/// so no attacker-controlled byte can reach a shipped finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptInjectionCircuit {
    /// Owner kind: `coordinator`, `worker`, or `execution_task`.
    pub owner_kind: String,
    /// Owner generation fence.
    pub owner_generation: u64,
    /// Canonical capability identity, e.g. `mcp:6:search:5:query`.
    pub capability: String,
    /// Tool call that produced the triggering assessment.
    pub tool_call_uid: String,
    /// Typed assessment class that caused the transition.
    pub assessment_class: String,
    /// Detector policy revision.
    pub detector_revision: String,
    /// Stage before the transition.
    pub prior_stage: String,
    /// Stage reached by the transition.
    pub reached_stage: String,
    /// Accumulated score before the transition.
    pub prior_score: u32,
    /// Accumulated score after the transition.
    pub reached_score: u32,
    /// Stable detector signals behind the assessment.
    pub signals: Vec<String>,
}

/// OCSF Entity Management event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityManagementEvent {
    /// OCSF class UID.
    pub class_uid: i32,
    /// OCSF class name.
    pub class_name: String,
    /// OCSF category UID.
    pub category_uid: i32,
    /// OCSF category name.
    pub category_name: String,
    /// OCSF type UID.
    pub type_uid: i64,
    /// Activity ID.
    pub activity_id: i32,
    /// Activity name.
    pub activity_name: String,
    /// Severity ID.
    pub severity_id: i32,
    /// Severity label.
    pub severity: String,
    /// Event occurrence time.
    pub time: DateTime<Utc>,
    /// OCSF metadata.
    pub metadata: Metadata,
    /// Actor.
    pub actor: Actor,
    /// Managed entity.
    pub entity: Resource,
    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}
