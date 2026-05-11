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
