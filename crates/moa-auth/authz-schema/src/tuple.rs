//! Typed OpenFGA tuple keys for MOA authorization.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// The full enumeration of object types in schema v1.
///
/// OpenFGA receives these as strings at the wire boundary, while Rust call
/// sites use this enum to avoid ad hoc object-type literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectType {
    /// A tenant/team boundary.
    Tenant,
    /// A workspace owned by a tenant.
    Workspace,
    /// A user or agent session.
    Session,
    /// A knowledge base inside a workspace.
    KnowledgeBase,
    /// A document inside a knowledge base.
    Document,
    /// A local API key principal.
    ApiKey,
    /// An AI agent principal.
    Agent,
}

impl fmt::Display for ObjectType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Tenant => "tenant",
            Self::Workspace => "workspace",
            Self::Session => "session",
            Self::KnowledgeBase => "knowledge_base",
            Self::Document => "document",
            Self::ApiKey => "api_key",
            Self::Agent => "agent",
        };
        f.write_str(value)
    }
}

/// Subject types on the user side of an OpenFGA tuple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserType {
    /// A human user.
    User,
    /// An AI agent principal.
    Agent,
    /// A local API key principal.
    ApiKey,
}

impl fmt::Display for UserType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::ApiKey => "api_key",
        };
        f.write_str(value)
    }
}

/// Relations defined in schema v1.
///
/// The caller is responsible for choosing a relation that exists on the target
/// object type. `moa-authz` enforces that at runtime; a fully type-safe
/// per-object relation enum is deferred until relation drift becomes a real
/// maintenance problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// Tenant membership.
    Member,
    /// Administrative access.
    Admin,
    /// Billing administration.
    BillingAdmin,
    /// SCIM provisioning administration.
    ScimAdmin,
    /// Workspace editing.
    Editor,
    /// Direct ownership.
    Owner,
    /// Session participation.
    Participant,
    /// Read access.
    Reader,
    /// Write access.
    Writer,
    /// Agent operator relationship.
    Operator,
    /// Delegation relationship for agent impersonation.
    CanActAs,
    /// Parent tenant relationship.
    Tenant,
    /// Parent workspace relationship.
    Workspace,
    /// Parent knowledge-base relationship.
    KnowledgeBase,
    /// API-key principal alias relationship.
    Principal,
}

impl fmt::Display for Relation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Member => "member",
            Self::Admin => "admin",
            Self::BillingAdmin => "billing_admin",
            Self::ScimAdmin => "scim_admin",
            Self::Editor => "editor",
            Self::Owner => "owner",
            Self::Participant => "participant",
            Self::Reader => "reader",
            Self::Writer => "writer",
            Self::Operator => "operator",
            Self::CanActAs => "can_act_as",
            Self::Tenant => "tenant",
            Self::Workspace => "workspace",
            Self::KnowledgeBase => "knowledge_base",
            Self::Principal => "principal",
        };
        f.write_str(value)
    }
}

/// A fully qualified OpenFGA tuple key.
///
/// The tuple relates a subject (`user_type`, `user_id`) to an object
/// (`object_type`, `object_id`) by a named relation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TupleKey {
    /// Subject type.
    pub user_type: UserType,
    /// Subject ID.
    pub user_id: Uuid,
    /// Relation between subject and object.
    pub relation: Relation,
    /// Object type.
    pub object_type: ObjectType,
    /// Object ID.
    pub object_id: Uuid,
}

impl TupleKey {
    /// Build a tuple key from typed subject, relation, and object components.
    pub fn new(
        user_type: UserType,
        user_id: Uuid,
        relation: Relation,
        object_type: ObjectType,
        object_id: Uuid,
    ) -> Self {
        Self {
            user_type,
            user_id,
            relation,
            object_type,
            object_id,
        }
    }

    /// Render the wire-format subject string, such as `user:<uuid>`.
    pub fn user_wire(&self) -> String {
        format!("{}:{}", self.user_type, self.user_id)
    }

    /// Render the wire-format object string, such as `workspace:<uuid>`.
    pub fn object_wire(&self) -> String {
        format!("{}:{}", self.object_type, self.object_id)
    }

    /// Render this tuple as a serializable OpenFGA Write API tuple key.
    pub fn to_wire(&self) -> TupleKeyWire {
        TupleKeyWire {
            user: self.user_wire(),
            relation: self.relation.to_string(),
            object: self.object_wire(),
        }
    }

    /// Build the deterministic outbox idempotency key for this tuple.
    ///
    /// Format:
    /// `{op}-{object_type}-{object_id}-{relation}-{user_type}-{user_id}-v{model_version}`.
    pub fn idempotency_key(&self, op: TupleOp, model_version: u32) -> String {
        format!(
            "{}-{}-{}-{}-{}-{}-v{}",
            op,
            self.object_type,
            self.object_id,
            self.relation,
            self.user_type,
            self.user_id,
            model_version,
        )
    }
}

/// Serializable OpenFGA tuple key shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TupleKeyWire {
    /// Wire-format subject, such as `user:<uuid>`.
    pub user: String,
    /// Relation name.
    pub relation: String,
    /// Wire-format object, such as `workspace:<uuid>`.
    pub object: String,
}

/// Tuple mutation operation for idempotency-key construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TupleOp {
    /// Write a tuple.
    Write,
    /// Delete a tuple.
    Delete,
}

impl fmt::Display for TupleOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Write => "write",
            Self::Delete => "delete",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SCHEMA_V1_JSON;

    #[test]
    fn tuple_wire_format_user_to_workspace() {
        // Pins: tuple construction renders stable OpenFGA subject/object wire IDs.
        let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("fixture user UUID should parse");
        let workspace_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222")
            .expect("fixture workspace UUID should parse");
        let tuple = TupleKey::new(
            UserType::User,
            user_id,
            Relation::Editor,
            ObjectType::Workspace,
            workspace_id,
        );

        assert_eq!(
            tuple.user_wire(),
            "user:11111111-1111-1111-1111-111111111111"
        );
        assert_eq!(
            tuple.object_wire(),
            "workspace:22222222-2222-2222-2222-222222222222"
        );
    }

    #[test]
    fn idempotency_key_is_deterministic_and_includes_model_version() {
        // Pins: outbox idempotency keys include operation, tuple identity, and model version.
        let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("fixture user UUID should parse");
        let workspace_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222")
            .expect("fixture workspace UUID should parse");
        let tuple = TupleKey::new(
            UserType::User,
            user_id,
            Relation::Editor,
            ObjectType::Workspace,
            workspace_id,
        );

        let first = tuple.idempotency_key(TupleOp::Write, 1);
        let second = tuple.idempotency_key(TupleOp::Write, 1);
        assert_eq!(first, second);
        assert_eq!(
            first,
            "write-workspace-22222222-2222-2222-2222-222222222222-editor-user-11111111-1111-1111-1111-111111111111-v1"
        );

        let v2 = tuple.idempotency_key(TupleOp::Write, 2);
        assert_ne!(first, v2);
    }

    #[test]
    fn schema_v1_json_contains_security_contract_types_and_delegation() {
        // Pins: the deployed OpenFGA JSON model includes the auth object set and agent delegation.
        let schema: serde_json::Value =
            serde_json::from_str(SCHEMA_V1_JSON).expect("schema_v1.json must parse");
        assert_eq!(schema["schema_version"], "1.2");

        let definitions = schema["type_definitions"]
            .as_array()
            .expect("schema_v1.json type_definitions must be an array");
        let mut types = definitions
            .iter()
            .map(|definition| {
                definition["type"]
                    .as_str()
                    .expect("type definition must include a type")
            })
            .collect::<Vec<_>>();
        types.sort_unstable();
        assert_eq!(
            types,
            [
                "agent",
                "api_key",
                "document",
                "knowledge_base",
                "session",
                "tenant",
                "user",
                "workspace",
            ]
        );

        let agent = definitions
            .iter()
            .find(|definition| definition["type"] == "agent")
            .expect("schema_v1.json must define agent");
        let agent_relations = agent["relations"]
            .as_object()
            .expect("agent relations must be an object");
        assert!(agent_relations.contains_key("can_act_as"));

        let workspace = definitions
            .iter()
            .find(|definition| definition["type"] == "workspace")
            .expect("schema_v1.json must define workspace");
        let workspace_relations = workspace["relations"]
            .as_object()
            .expect("workspace relations must be an object");
        for relation in ["tenant", "admin", "editor", "member"] {
            assert!(
                workspace_relations.contains_key(relation),
                "workspace must define relation {relation}"
            );
        }

        let session = definitions
            .iter()
            .find(|definition| definition["type"] == "session")
            .expect("schema_v1.json must define session");
        let session_relations = session["relations"]
            .as_object()
            .expect("session relations must be an object");
        for relation in ["workspace", "owner", "participant"] {
            assert!(
                session_relations.contains_key(relation),
                "session must define relation {relation}"
            );
        }
    }
}
