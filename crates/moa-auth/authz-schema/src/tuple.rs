//! Typed OpenFGA tuple keys for MOA authorization.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The full enumeration of object types in schema v1.
///
/// OpenFGA receives these as strings at the wire boundary, while Rust call
/// sites use this enum to avoid ad hoc object-type literals.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, strum::Display,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ObjectType {
    /// A deployment-level administration boundary.
    Workspace,
    /// A tenant runtime boundary.
    Tenant,
    /// A tenant-local end-user contact.
    Contact,
    /// A tenant-local operator, contact, or agent session.
    Session,
    /// A local API key principal.
    ApiKey,
    /// An AI agent principal.
    Agent,
    /// A tenant-owned installed connector account.
    ConnectorConnection,
    /// Durable tenant-owned sandbox filesystem state.
    SandboxWorkspace,
}

/// Subject types on the subject side of an OpenFGA tuple.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, strum::Display,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum UserType {
    /// A deployment workspace object used as a tuple subject.
    Workspace,
    /// A tenant object used as a parent tuple subject.
    Tenant,
    /// A session object used as a workspace-scope tuple subject.
    Session,
    /// A human operator.
    Operator,
    /// A service principal.
    Service,
    /// A tenant-local end-user contact.
    Contact,
    /// An AI agent principal.
    Agent,
    /// A local API key principal.
    ApiKey,
}

/// Relations defined in schema v1.
///
/// The caller is responsible for choosing a relation that exists on the target
/// object type. `moa-authz` enforces that at runtime; a fully type-safe
/// per-object relation enum is deferred until relation drift becomes a real
/// maintenance problem.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, strum::Display,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Relation {
    /// Administrative access.
    Admin,
    /// Tenant or agent operation access.
    Operator,
    /// Direct ownership.
    Owner,
    /// Session participation.
    Participant,
    /// Delegation relationship for agent impersonation.
    CanActAs,
    /// Parent tenant relationship.
    Tenant,
    /// Owning session relationship.
    Session,
    /// Parent workspace relationship.
    Workspace,
    /// Contact object relationship.
    Contact,
    /// Administrative control of a tenant-owned resource.
    Manage,
    /// Permission to invoke or otherwise consume a tenant-owned resource.
    Use,
}

/// A fully qualified OpenFGA tuple key.
///
/// The tuple relates a subject (`user_type`, `user_id`) to an object
/// (`object_type`, `object_id`) by a named relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

    /// Render the wire-format subject string, such as `operator:<uuid>`.
    pub fn user_wire(&self) -> String {
        format!("{}:{}", self.user_type, self.user_id)
    }

    /// Render the wire-format object string, such as `tenant:<uuid>`.
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
}

/// Serializable OpenFGA tuple key shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TupleKeyWire {
    /// Wire-format subject, such as `operator:<uuid>`.
    pub user: String,
    /// Relation name.
    pub relation: String,
    /// Wire-format object, such as `tenant:<uuid>`.
    pub object: String,
}

/// Tuple mutation operation for idempotency-key construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum TupleOp {
    /// Write a tuple.
    Write,
    /// Delete a tuple.
    Delete,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MODEL_VERSION, SCHEMA_V1_JSON};

    #[test]
    fn display_strings_are_pinned() {
        // Pins: strum-derived `Display` output must stay byte-identical to the
        // previous hand-written tables, since these strings cross the OpenFGA
        // wire boundary and are baked into outbox idempotency keys.
        let object_types = [
            (ObjectType::Workspace, "workspace"),
            (ObjectType::Tenant, "tenant"),
            (ObjectType::Contact, "contact"),
            (ObjectType::Session, "session"),
            (ObjectType::ApiKey, "api_key"),
            (ObjectType::Agent, "agent"),
            (ObjectType::ConnectorConnection, "connector_connection"),
            (ObjectType::SandboxWorkspace, "sandbox_workspace"),
        ];
        for (value, label) in object_types {
            assert_eq!(value.to_string(), label);
        }

        let user_types = [
            (UserType::Workspace, "workspace"),
            (UserType::Tenant, "tenant"),
            (UserType::Session, "session"),
            (UserType::Operator, "operator"),
            (UserType::Service, "service"),
            (UserType::Contact, "contact"),
            (UserType::Agent, "agent"),
            (UserType::ApiKey, "api_key"),
        ];
        for (value, label) in user_types {
            assert_eq!(value.to_string(), label);
        }

        let relations = [
            (Relation::Admin, "admin"),
            (Relation::Operator, "operator"),
            (Relation::Owner, "owner"),
            (Relation::Participant, "participant"),
            (Relation::CanActAs, "can_act_as"),
            (Relation::Tenant, "tenant"),
            (Relation::Session, "session"),
            (Relation::Workspace, "workspace"),
            (Relation::Contact, "contact"),
            (Relation::Manage, "manage"),
            (Relation::Use, "use"),
        ];
        for (value, label) in relations {
            assert_eq!(value.to_string(), label);
        }

        let tuple_ops = [(TupleOp::Write, "write"), (TupleOp::Delete, "delete")];
        for (value, label) in tuple_ops {
            assert_eq!(value.to_string(), label);
        }
    }

    #[test]
    fn tuple_wire_format_contact_to_session() {
        // Pins: tuple construction renders stable OpenFGA subject/object wire IDs.
        let contact_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("fixture contact UUID should parse");
        let session_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222")
            .expect("fixture session UUID should parse");
        let tuple = TupleKey::new(
            UserType::Contact,
            contact_id,
            Relation::Participant,
            ObjectType::Session,
            session_id,
        );

        assert_eq!(
            tuple.user_wire(),
            "contact:11111111-1111-1111-1111-111111111111"
        );
        assert_eq!(
            tuple.object_wire(),
            "session:22222222-2222-2222-2222-222222222222"
        );
    }

    #[test]
    fn tuple_wire_format_workspace_to_tenant() {
        // Pins: workspace admin inheritance uses a workspace object as the tuple subject.
        let workspace_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("fixture workspace UUID should parse");
        let tenant_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222")
            .expect("fixture tenant UUID should parse");
        let tuple = TupleKey::new(
            UserType::Workspace,
            workspace_id,
            Relation::Workspace,
            ObjectType::Tenant,
            tenant_id,
        );

        assert_eq!(
            tuple.user_wire(),
            "workspace:11111111-1111-1111-1111-111111111111"
        );
        assert_eq!(
            tuple.object_wire(),
            "tenant:22222222-2222-2222-2222-222222222222"
        );
        assert_eq!(tuple.to_wire().relation, "workspace");
    }

    #[test]
    fn tuple_wire_format_agent_use_to_connector_connection() {
        // Pins: connector grants use the typed connection object and `use`
        // relation, so outbox identities cannot drift to ad hoc wire strings.
        let agent_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("fixture agent UUID should parse");
        let connection_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222")
            .expect("fixture connector connection UUID should parse");
        let tuple = TupleKey::new(
            UserType::Agent,
            agent_id,
            Relation::Use,
            ObjectType::ConnectorConnection,
            connection_id,
        );

        assert_eq!(
            tuple.to_wire(),
            TupleKeyWire {
                user: "agent:11111111-1111-1111-1111-111111111111".to_string(),
                relation: "use".to_string(),
                object: "connector_connection:22222222-2222-2222-2222-222222222222".to_string(),
            }
        );
    }

    #[test]
    fn schema_v1_json_contains_security_contract_types_and_delegation() {
        // Pins: the deployed OpenFGA JSON model includes the auth object set and agent delegation.
        assert_eq!(
            MODEL_VERSION, 7,
            "sandbox workspace relations require a new outbox model version"
        );
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
                "connector_connection",
                "contact",
                "operator",
                "sandbox_workspace",
                "service",
                "session",
                "tenant",
                "workspace",
            ]
        );

        let workspace = definitions
            .iter()
            .find(|definition| definition["type"] == "workspace")
            .expect("schema_v1.json must define workspace");
        let workspace_relations = workspace["relations"]
            .as_object()
            .expect("workspace relations must be an object");
        assert!(workspace_relations.contains_key("admin"));

        let agent = definitions
            .iter()
            .find(|definition| definition["type"] == "agent")
            .expect("schema_v1.json must define agent");
        let agent_relations = agent["relations"]
            .as_object()
            .expect("agent relations must be an object");
        assert!(agent_relations.contains_key("can_act_as"));

        let tenant = definitions
            .iter()
            .find(|definition| definition["type"] == "tenant")
            .expect("schema_v1.json must define tenant");
        let tenant_relations = tenant["relations"]
            .as_object()
            .expect("tenant relations must be an object");
        for relation in ["admin", "operator", "workspace"] {
            assert!(
                tenant_relations.contains_key(relation),
                "tenant must define relation {relation}"
            );
        }
        assert_eq!(
            tenant_relations["admin"]["union"]["child"][1]["tupleToUserset"]["tupleset"]["relation"],
            "workspace"
        );

        let session = definitions
            .iter()
            .find(|definition| definition["type"] == "session")
            .expect("schema_v1.json must define session");
        let session_relations = session["relations"]
            .as_object()
            .expect("session relations must be an object");
        for relation in ["tenant", "contact", "owner", "participant"] {
            assert!(
                session_relations.contains_key(relation),
                "session must define relation {relation}"
            );
        }

        // Pins: the session's bound contact is a participant via a same-object
        // computed userset, not a reflexive tuple-to-userset that OpenFGA can
        // never satisfy. A `tupleToUserset` on `contact->contact` would require
        // a (contact:X, contact, contact:X) tuple nothing writes.
        let participant_children = session_relations["participant"]["union"]["child"]
            .as_array()
            .expect("participant must be a union of children");
        assert!(
            participant_children.iter().any(|child| {
                child["computedUserset"]["relation"] == "contact"
                    && child.get("tupleToUserset").is_none()
            }),
            "participant must grant the session contact via a same-object computed userset"
        );
    }

    #[test]
    fn connector_connection_schema_pins_manage_and_use_inheritance() {
        // Pins: an installed connector is managed only by its owner or tenant
        // administrators, while use is an explicit contact/agent/operator grant
        // or inherited from manage.
        let schema: serde_json::Value =
            serde_json::from_str(SCHEMA_V1_JSON).expect("schema_v1.json must parse");
        let definitions = schema["type_definitions"]
            .as_array()
            .expect("schema_v1.json type_definitions must be an array");
        let connection = definitions
            .iter()
            .find(|definition| definition["type"] == "connector_connection")
            .expect("schema_v1.json must define connector_connection");

        assert_eq!(
            connection["metadata"]["relations"],
            serde_json::json!({
                "tenant": {
                    "directly_related_user_types": [{ "type": "tenant" }]
                },
                "owner": {
                    "directly_related_user_types": [{ "type": "operator" }]
                },
                "use": {
                    "directly_related_user_types": [
                        { "type": "contact" },
                        { "type": "agent" },
                        { "type": "operator" }
                    ]
                }
            })
        );
        assert_eq!(
            connection["relations"],
            serde_json::json!({
                "tenant": { "this": {} },
                "owner": { "this": {} },
                "manage": {
                    "union": {
                        "child": [
                            { "computedUserset": { "relation": "owner" } },
                            {
                                "tupleToUserset": {
                                    "tupleset": { "relation": "tenant" },
                                    "computedUserset": { "relation": "admin" }
                                }
                            }
                        ]
                    }
                },
                "use": {
                    "union": {
                        "child": [
                            { "this": {} },
                            { "computedUserset": { "relation": "manage" } }
                        ]
                    }
                }
            })
        );
    }

    #[test]
    fn sandbox_workspace_tuple_wire_formats_parent_scope_and_agent_use() {
        // Pins: the desired-tuple ledger can construct the tenant parent,
        // session scope, and explicit delegated-agent use grants without ad
        // hoc OpenFGA subject or object strings.
        let subject_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("fixture subject UUID should parse");
        let workspace_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222")
            .expect("fixture sandbox workspace UUID should parse");

        let tuples = [
            TupleKey::new(
                UserType::Tenant,
                subject_id,
                Relation::Tenant,
                ObjectType::SandboxWorkspace,
                workspace_id,
            ),
            TupleKey::new(
                UserType::Session,
                subject_id,
                Relation::Session,
                ObjectType::SandboxWorkspace,
                workspace_id,
            ),
            TupleKey::new(
                UserType::Agent,
                subject_id,
                Relation::Use,
                ObjectType::SandboxWorkspace,
                workspace_id,
            ),
        ];

        assert_eq!(
            tuples.map(|tuple| tuple.to_wire()),
            [
                TupleKeyWire {
                    user: "tenant:11111111-1111-1111-1111-111111111111".to_string(),
                    relation: "tenant".to_string(),
                    object: "sandbox_workspace:22222222-2222-2222-2222-222222222222".to_string(),
                },
                TupleKeyWire {
                    user: "session:11111111-1111-1111-1111-111111111111".to_string(),
                    relation: "session".to_string(),
                    object: "sandbox_workspace:22222222-2222-2222-2222-222222222222".to_string(),
                },
                TupleKeyWire {
                    user: "agent:11111111-1111-1111-1111-111111111111".to_string(),
                    relation: "use".to_string(),
                    object: "sandbox_workspace:22222222-2222-2222-2222-222222222222".to_string(),
                },
            ]
        );
    }

    #[test]
    fn sandbox_workspace_schema_pins_private_use_and_admin_management() {
        // Pins: a workspace binds its session separately from explicit owner
        // grants, tenant admins inherit management, and agents receive no use
        // path other than a direct workspace grant checked alongside
        // `can_act_as` by the delegated authorization helper.
        let schema: serde_json::Value =
            serde_json::from_str(SCHEMA_V1_JSON).expect("schema_v1.json must parse");
        let definitions = schema["type_definitions"]
            .as_array()
            .expect("schema_v1.json type_definitions must be an array");
        let workspace = definitions
            .iter()
            .find(|definition| definition["type"] == "sandbox_workspace")
            .expect("schema_v1.json must define sandbox_workspace");

        assert_eq!(
            workspace["metadata"]["relations"],
            serde_json::json!({
                "tenant": {
                    "directly_related_user_types": [{ "type": "tenant" }]
                },
                "session": {
                    "directly_related_user_types": [{ "type": "session" }]
                },
                "owner": {
                    "directly_related_user_types": [
                        { "type": "contact" },
                        { "type": "operator" },
                        { "type": "api_key" }
                    ]
                },
                "manage": {
                    "directly_related_user_types": [
                        { "type": "operator" },
                        { "type": "api_key" }
                    ]
                },
                "use": {
                    "directly_related_user_types": [
                        { "type": "contact" },
                        { "type": "agent" },
                        { "type": "operator" },
                        { "type": "api_key" }
                    ]
                }
            })
        );
        assert_eq!(
            workspace["relations"],
            serde_json::json!({
                "tenant": { "this": {} },
                "session": { "this": {} },
                "owner": { "this": {} },
                "manage": {
                    "union": {
                        "child": [
                            { "this": {} },
                            { "computedUserset": { "relation": "owner" } },
                            {
                                "tupleToUserset": {
                                    "tupleset": { "relation": "tenant" },
                                    "computedUserset": { "relation": "admin" }
                                }
                            }
                        ]
                    }
                },
                "use": {
                    "union": {
                        "child": [
                            { "this": {} },
                            { "computedUserset": { "relation": "manage" } }
                        ]
                    }
                }
            })
        );
    }
}
