//! Model-facing worker tool kinds, parsers, and JSON schemas.

use serde::de::DeserializeOwned;

use crate::error::{MoaError, Result};

use super::super::completion::ToolInvocation;
use super::commands::{
    CancelWorkerInput, ListWorkersInput, MessageWorkerInput, ProvideWorkerInputInput,
    ReportToParentInput, RequestInputInput, SpawnWorkerInput, WaitWorkerInput,
};

/// Stable kind for one built-in worker delegation tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationToolKind {
    /// Child spawn tool.
    Spawn,
    /// Child wait tool.
    Wait,
    /// Follow-up message tool.
    Message,
    /// Child-listing tool.
    List,
    /// Child cancellation tool.
    Cancel,
    /// Provide-input tool answering a child `request_input` round-trip.
    ProvideInput,
}

impl DelegationToolKind {
    /// All built-in delegation tool kinds in stable prompt order.
    pub const ALL: [Self; 6] = [
        Self::Spawn,
        Self::Wait,
        Self::Message,
        Self::List,
        Self::Cancel,
        Self::ProvideInput,
    ];

    /// Returns the stable provider-facing tool name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Spawn => "spawn_worker",
            Self::Wait => "wait_worker",
            Self::Message => "message_worker",
            Self::List => "list_workers",
            Self::Cancel => "cancel_worker",
            Self::ProvideInput => "provide_worker_input",
        }
    }

    /// Returns the kind for a provider-facing delegation tool name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Returns the provider-facing JSON schema for this tool.
    #[must_use]
    pub fn schema(self) -> serde_json::Value {
        match self {
            Self::Spawn => spawn_worker_tool_schema(),
            Self::Wait => wait_worker_tool_schema(),
            Self::Message => message_worker_tool_schema(),
            Self::List => list_workers_tool_schema(),
            Self::Cancel => cancel_worker_tool_schema(),
            Self::ProvideInput => provide_worker_input_tool_schema(),
        }
    }
}

/// Parsed payload for one built-in worker delegation tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationTool {
    /// Child spawn payload.
    Spawn(SpawnWorkerInput),
    /// Child wait payload.
    Wait(WaitWorkerInput),
    /// Follow-up message payload.
    Message(MessageWorkerInput),
    /// Child-listing payload.
    List(ListWorkersInput),
    /// Child cancellation payload.
    Cancel(CancelWorkerInput),
    /// Provide-input payload answering a child `request_input` round-trip.
    ProvideInput(ProvideWorkerInputInput),
}

impl DelegationTool {
    /// Parses a provider invocation into a typed delegation tool when recognized.
    pub fn from_invocation(invocation: &ToolInvocation) -> Result<Option<Self>> {
        let Some(kind) = DelegationToolKind::from_name(&invocation.name) else {
            return Ok(None);
        };

        Ok(Some(match kind {
            DelegationToolKind::Spawn => Self::Spawn(parse_delegation_tool_input(invocation)?),
            DelegationToolKind::Wait => Self::Wait(parse_delegation_tool_input(invocation)?),
            DelegationToolKind::Message => Self::Message(parse_delegation_tool_input(invocation)?),
            DelegationToolKind::List => Self::List(parse_delegation_tool_input(invocation)?),
            DelegationToolKind::Cancel => Self::Cancel(parse_delegation_tool_input(invocation)?),
            DelegationToolKind::ProvideInput => {
                Self::ProvideInput(parse_delegation_tool_input(invocation)?)
            }
        }))
    }

    /// Returns the parsed tool kind.
    #[must_use]
    pub fn kind(&self) -> DelegationToolKind {
        match self {
            Self::Spawn(_) => DelegationToolKind::Spawn,
            Self::Wait(_) => DelegationToolKind::Wait,
            Self::Message(_) => DelegationToolKind::Message,
            Self::List(_) => DelegationToolKind::List,
            Self::Cancel(_) => DelegationToolKind::Cancel,
            Self::ProvideInput(_) => DelegationToolKind::ProvideInput,
        }
    }

    /// Returns the stable provider-facing tool name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.kind().name()
    }
}

/// Stable `spawn_worker` tool schema.
pub fn spawn_worker_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "spawn_worker",
        "description": "Delegate bounded, general-purpose work to a child agent when the coordinator can give it enough context to run independently and return evidence for synthesis. Good fits include research, reports, comparisons, audits, incident investigations, option checks, or named workstreams that can run in parallel. Spawn ready work before final synthesis, and wait only when another step depends on a worker result.",
        "input_schema": {
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Full delegated instruction for the child. Include the purpose, relevant context, expected output, evidence needs, constraints, and any relevant skill steps the child should follow."
                },
                "tool_subset": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Subset of tool names the child may use."
                },
                "budget_tokens": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Token budget reserved for the child agent."
                },
                "max_turns": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum autonomous turns the child may run for this task."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        }
    })
}

/// Stable `wait_worker` tool schema.
pub fn wait_worker_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "wait_worker",
        "description": "Wait briefly for a previously spawned worker to finish and return its current status or terminal result.",
        "input_schema": {
            "type": "object",
            "properties": {
                "worker_id": {
                    "type": "string",
                    "description": "Worker id returned by spawn_worker."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 30000,
                    "description": "Maximum wait time in milliseconds."
                }
            },
            "required": ["worker_id"],
            "additionalProperties": false
        }
    })
}

/// Stable `message_worker` tool schema.
pub fn message_worker_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "message_worker",
        "description": "Send a follow-up instruction to a running or resident worker.",
        "input_schema": {
            "type": "object",
            "properties": {
                "worker_id": {
                    "type": "string",
                    "description": "Worker id returned by spawn_worker."
                },
                "text": {
                    "type": "string",
                    "description": "Follow-up instruction for the child agent."
                }
            },
            "required": ["worker_id", "text"],
            "additionalProperties": false
        }
    })
}

/// Stable `list_workers` tool schema.
pub fn list_workers_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "list_workers",
        "description": "List child workers owned by the current agent and their current statuses.",
        "input_schema": {
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }
    })
}

/// Stable `cancel_worker` tool schema.
pub fn cancel_worker_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "cancel_worker",
        "description": "Cancel a previously spawned child worker.",
        "input_schema": {
            "type": "object",
            "properties": {
                "worker_id": {
                    "type": "string",
                    "description": "Worker id returned by spawn_worker."
                },
                "reason": {
                    "type": "string",
                    "description": "Short cancellation reason."
                }
            },
            "required": ["worker_id"],
            "additionalProperties": false
        }
    })
}

/// Stable `provide_worker_input` tool schema (coordinator/parent side).
pub fn provide_worker_input_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "provide_worker_input",
        "description": "Answer a worker that requested input (a needs_input signal), unblocking it.",
        "input_schema": {
            "type": "object",
            "properties": {
                "worker_id": {
                    "type": "string",
                    "description": "Worker id that raised the needs_input request."
                },
                "input_request_id": {
                    "type": "string",
                    "description": "input_request_id carried by the needs_input signal."
                },
                "text": {
                    "type": "string",
                    "description": "Answer text delivered to the blocked worker."
                }
            },
            "required": ["worker_id", "input_request_id", "text"],
            "additionalProperties": false
        }
    })
}

/// Returns all delegation tool schemas.
pub fn delegation_tool_schemas() -> Vec<serde_json::Value> {
    DelegationToolKind::ALL
        .into_iter()
        .map(DelegationToolKind::schema)
        .collect()
}

/// Stable kind for one child-only model-driven report tool.
///
/// These are distinct from delegation tools: delegation tools *manage* children,
/// while child-report tools let a child *communicate upward* to its coordinator.
/// They are exposed only inside the worker tool subset, never on the root
/// session, and are handled inside the child's own turn loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildReportToolKind {
    /// Report a finding or a blocking condition to the coordinator.
    Report,
    /// Request input from the coordinator (or, via it, the user), blocking the child.
    RequestInput,
}

impl ChildReportToolKind {
    /// All child-report tool kinds in stable prompt order.
    pub const ALL: [Self; 2] = [Self::Report, Self::RequestInput];

    /// Returns the stable provider-facing tool name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Report => "report_to_parent",
            Self::RequestInput => "request_input",
        }
    }

    /// Returns the kind for a provider-facing child-report tool name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Returns the provider-facing JSON schema for this tool.
    #[must_use]
    pub fn schema(self) -> serde_json::Value {
        match self {
            Self::Report => report_to_parent_tool_schema(),
            Self::RequestInput => request_input_tool_schema(),
        }
    }
}

/// Parsed payload for one child-only report tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildReportTool {
    /// `report_to_parent` payload.
    Report(ReportToParentInput),
    /// `request_input` payload.
    RequestInput(RequestInputInput),
}

impl ChildReportTool {
    /// Parses a provider invocation into a typed child-report tool when recognized.
    pub fn from_invocation(invocation: &ToolInvocation) -> Result<Option<Self>> {
        let Some(kind) = ChildReportToolKind::from_name(&invocation.name) else {
            return Ok(None);
        };
        Ok(Some(match kind {
            ChildReportToolKind::Report => Self::Report(parse_delegation_tool_input(invocation)?),
            ChildReportToolKind::RequestInput => {
                Self::RequestInput(parse_delegation_tool_input(invocation)?)
            }
        }))
    }

    /// Returns the parsed tool kind.
    #[must_use]
    pub fn kind(&self) -> ChildReportToolKind {
        match self {
            Self::Report(_) => ChildReportToolKind::Report,
            Self::RequestInput(_) => ChildReportToolKind::RequestInput,
        }
    }

    /// Returns the stable provider-facing tool name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.kind().name()
    }
}

/// Stable `report_to_parent` tool schema (child-only).
pub fn report_to_parent_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "report_to_parent",
        "description": "Report a finding (non-blocking) or a blocking condition to the coordinator. Use sparingly for attention-worthy events, not routine progress.",
        "input_schema": {
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["finding", "blocked"],
                    "description": "finding records without interrupting the coordinator; blocked asks an idle coordinator to step in."
                },
                "summary": {
                    "type": "string",
                    "description": "Short, safe one-line summary surfaced to the coordinator."
                }
            },
            "required": ["kind", "summary"],
            "additionalProperties": false
        }
    })
}

/// Stable `request_input` tool schema (child-only).
pub fn request_input_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "request_input",
        "description": "Ask the coordinator (or, via it, the user) a question and block until an answer arrives or the request times out.",
        "input_schema": {
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question that must be answered before the child can continue."
                },
                "audience": {
                    "type": "string",
                    "enum": ["coordinator", "user"],
                    "description": "coordinator if the orchestrating agent can answer; user if a human must."
                }
            },
            "required": ["question"],
            "additionalProperties": false
        }
    })
}

/// Returns all child-only report tool schemas (exposed inside the worker tool subset only).
pub fn child_report_tool_schemas() -> Vec<serde_json::Value> {
    ChildReportToolKind::ALL
        .into_iter()
        .map(ChildReportToolKind::schema)
        .collect()
}

/// Returns whether `name` is one of MOA's child-only report tools.
pub fn is_child_report_tool_name(name: &str) -> bool {
    ChildReportToolKind::from_name(name).is_some()
}

/// Returns one delegation tool schema by name.
pub fn delegation_tool_schema(name: &str) -> Option<serde_json::Value> {
    DelegationToolKind::from_name(name).map(DelegationToolKind::schema)
}

/// Returns whether `name` is one of MOA's built-in delegation tools.
pub fn is_delegation_tool_name(name: &str) -> bool {
    DelegationToolKind::from_name(name).is_some()
}

/// Parses a delegation tool invocation input while preserving the tool name in errors.
pub fn parse_delegation_tool_input<T>(invocation: &ToolInvocation) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(invocation.input.clone()).map_err(|error| {
        MoaError::SerializationError(format!(
            "failed to deserialize {} input: {error}",
            invocation.name
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::worker::{commands::ChildReportKind, state::InputAudience};

    #[test]
    fn delegation_schema_names_are_stable() {
        // Pins: the model-facing delegation tool names remain stable.
        let names = delegation_tool_schemas()
            .into_iter()
            .map(|schema| {
                schema
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .expect("delegation schema should have a string name")
                    .to_string()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "spawn_worker",
                "wait_worker",
                "message_worker",
                "list_workers",
                "cancel_worker",
                "provide_worker_input",
            ]
        );
    }

    #[test]
    fn stable_delegation_names_map_to_expected_kind() {
        // Pins: each stable tool name remains classified under the intended delegation kind.
        let expected = [
            ("spawn_worker", DelegationToolKind::Spawn),
            ("wait_worker", DelegationToolKind::Wait),
            ("message_worker", DelegationToolKind::Message),
            ("list_workers", DelegationToolKind::List),
            ("cancel_worker", DelegationToolKind::Cancel),
            ("provide_worker_input", DelegationToolKind::ProvideInput),
        ];

        for (name, expected_kind) in expected {
            assert!(
                is_delegation_tool_name(name),
                "{name} should be recognized as a delegation tool"
            );
            let observed_kind = DelegationToolKind::from_name(name)
                .unwrap_or_else(|| panic!("{name} should map to a delegation kind"));
            assert_eq!(observed_kind, expected_kind, "{name} kind changed");
        }

        assert!(!is_delegation_tool_name("unknown_worker"));
        assert!(delegation_tool_schema("unknown_worker").is_none());
    }

    #[test]
    fn spawn_worker_schema_describes_general_purpose_delegation() {
        // Pins: the model-facing spawn tool presents workers as bounded,
        // general-purpose delegation, not code-specific sidekick behavior.
        let schema = spawn_worker_tool_schema();
        let description = schema
            .get("description")
            .and_then(serde_json::Value::as_str)
            .expect("spawn_worker should have a description");
        assert!(description.contains("bounded, general-purpose work"));
        assert!(description.contains("return evidence for synthesis"));
        assert!(description.contains("research, reports, comparisons, audits"));
        assert!(description.contains("named workstreams"));
        assert!(description.contains("wait only when another step depends"));

        let properties = schema
            .pointer("/input_schema/properties")
            .and_then(serde_json::Value::as_object)
            .expect("spawn_worker should expose object properties");
        let task_description = properties
            .get("task")
            .and_then(|property| property.get("description"))
            .and_then(serde_json::Value::as_str)
            .expect("task should have a description");
        assert!(task_description.contains("purpose"));
        assert!(task_description.contains("relevant context"));
        assert!(task_description.contains("expected output"));
        assert!(task_description.contains("evidence needs"));
        assert!(task_description.contains("constraints"));
        assert!(task_description.contains("relevant skill steps"));
    }

    #[test]
    fn spawn_worker_schema_uses_single_task_contract() {
        // Pins: spawn_worker exposes one canonical task instruction plus bounded
        // execution controls, without redundant or speculative contract fields.
        let schema = spawn_worker_tool_schema();

        let required = schema
            .pointer("/input_schema/required")
            .and_then(serde_json::Value::as_array)
            .expect("spawn_worker should list required fields")
            .iter()
            .map(|field| {
                field
                    .as_str()
                    .expect("required field names should be strings")
            })
            .collect::<Vec<_>>();
        assert_eq!(required, vec!["task"]);

        let properties = schema
            .pointer("/input_schema/properties")
            .and_then(serde_json::Value::as_object)
            .expect("spawn_worker should expose object properties");
        let mut property_names = properties.keys().map(String::as_str).collect::<Vec<_>>();
        property_names.sort_unstable();
        assert_eq!(
            property_names,
            vec!["budget_tokens", "max_turns", "task", "tool_subset"]
        );

        for removed_field in [
            "task_name",
            "capability_mode",
            "output_contract",
            "knowledge_scope",
            "details",
        ] {
            assert!(
                !properties.contains_key(removed_field),
                "{removed_field} should not be part of the spawn_worker contract"
            );
        }
    }

    #[test]
    fn spawn_worker_schema_rejects_task_name() {
        // Pins: task_name is not accepted as a compatibility field by the
        // provider-facing schema or the typed spawn_worker parser.
        let schema = spawn_worker_tool_schema();
        let additional_properties = schema
            .pointer("/input_schema/additionalProperties")
            .and_then(serde_json::Value::as_bool)
            .expect("spawn_worker should specify additionalProperties");
        assert!(!additional_properties);

        let properties = schema
            .pointer("/input_schema/properties")
            .and_then(serde_json::Value::as_object)
            .expect("spawn_worker should expose object properties");
        assert!(!properties.contains_key("task_name"));

        let invocation = ToolInvocation {
            id: Some("spawn-worker-task-name".to_string()),
            name: "spawn_worker".to_string(),
            input: serde_json::json!({
                "task": "research",
                "task_name": "research-task"
            }),
        };

        let error = parse_delegation_tool_input::<SpawnWorkerInput>(&invocation)
            .expect_err("task_name should not be accepted by spawn_worker");
        let message = error.to_string();
        assert!(
            message.contains("task_name"),
            "error should name the rejected field, got: {message}"
        );
    }

    #[test]
    fn known_delegation_tool_parse_error_names_tool() {
        // Pins: delegation input parsing errors identify the offending tool call.
        let invocation = ToolInvocation {
            id: Some("toolu_1".to_string()),
            name: "spawn_worker".to_string(),
            input: serde_json::json!("not an object"),
        };

        let error = parse_delegation_tool_input::<SpawnWorkerInput>(&invocation)
            .expect_err("invalid spawn_worker input should fail");

        let message = error.to_string();
        assert!(
            message.contains("spawn_worker"),
            "error should name the tool, got: {message}"
        );
        assert!(
            message.contains("failed to deserialize"),
            "error should describe a parse failure, got: {message}"
        );
    }

    #[test]
    fn typed_delegation_parser_covers_every_builtin_tool() {
        // Pins: every built-in delegation tool has exactly one typed payload branch.
        let cases = [
            (
                "spawn_worker",
                serde_json::json!({
                    "task": "research",
                    "tool_subset": ["web_fetch"],
                    "budget_tokens": 123
                }),
                DelegationToolKind::Spawn,
            ),
            (
                "wait_worker",
                serde_json::json!({
                    "worker_id": "child-1",
                    "timeout_ms": 50
                }),
                DelegationToolKind::Wait,
            ),
            (
                "message_worker",
                serde_json::json!({
                    "worker_id": "child-1",
                    "text": "continue"
                }),
                DelegationToolKind::Message,
            ),
            (
                "list_workers",
                serde_json::json!({}),
                DelegationToolKind::List,
            ),
            (
                "cancel_worker",
                serde_json::json!({
                    "worker_id": "child-1",
                    "reason": "no longer needed"
                }),
                DelegationToolKind::Cancel,
            ),
            (
                "provide_worker_input",
                serde_json::json!({
                    "worker_id": "child-1",
                    "input_request_id": "req-1",
                    "text": "use staging credentials"
                }),
                DelegationToolKind::ProvideInput,
            ),
        ];

        for (name, input, expected_kind) in cases {
            let invocation = ToolInvocation {
                id: Some(format!("{name}-id")),
                name: name.to_string(),
                input,
            };
            let parsed = DelegationTool::from_invocation(&invocation)
                .expect("known delegation tool should parse")
                .unwrap_or_else(|| panic!("{name} should be recognized"));

            assert_eq!(parsed.kind(), expected_kind, "{name} parsed to wrong kind");
            assert_eq!(parsed.name(), name);
        }
    }

    #[test]
    fn typed_delegation_parser_ignores_unknown_tools() {
        // Pins: non-delegation tools stay on the regular tool-executor path.
        let invocation = ToolInvocation {
            id: Some("regular-tool".to_string()),
            name: "bash".to_string(),
            input: serde_json::json!({"cmd": "true"}),
        };

        assert_eq!(
            DelegationTool::from_invocation(&invocation).expect("unknown tool should not fail"),
            None
        );
    }

    #[test]
    fn child_report_tools_are_separate_from_delegation_tools() {
        // Pins: child-report tool names are not classified as delegation tools (they are
        // handled in the child's own turn loop, not the delegation manager path), and the
        // child-report schema set carries exactly the two child-only tools.
        assert!(is_child_report_tool_name("report_to_parent"));
        assert!(is_child_report_tool_name("request_input"));
        assert!(!is_delegation_tool_name("report_to_parent"));
        assert!(!is_delegation_tool_name("request_input"));
        assert!(!is_child_report_tool_name("spawn_worker"));

        let names = child_report_tool_schemas()
            .into_iter()
            .map(|schema| {
                schema
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .expect("child-report schema should have a string name")
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["report_to_parent", "request_input"]);
    }

    #[test]
    fn child_report_tools_parse_typed_payloads() {
        // Pins: report_to_parent and request_input parse into typed child-report payloads,
        // and request_input defaults the audience to the coordinator when omitted.
        let report = ChildReportTool::from_invocation(&ToolInvocation {
            id: Some("r1".to_string()),
            name: "report_to_parent".to_string(),
            input: serde_json::json!({"kind": "blocked", "summary": "needs credentials"}),
        })
        .expect("report tool should parse")
        .expect("report tool should be recognized");
        assert_eq!(report.kind(), ChildReportToolKind::Report);
        let ChildReportTool::Report(input) = report else {
            panic!("expected report payload");
        };
        assert_eq!(input.kind, ChildReportKind::Blocked);
        assert_eq!(input.summary, "needs credentials");

        let request = ChildReportTool::from_invocation(&ToolInvocation {
            id: Some("r2".to_string()),
            name: "request_input".to_string(),
            input: serde_json::json!({"question": "which environment?"}),
        })
        .expect("request_input should parse")
        .expect("request_input should be recognized");
        let ChildReportTool::RequestInput(input) = request else {
            panic!("expected request_input payload");
        };
        assert_eq!(input.question, "which environment?");
        assert_eq!(input.audience, InputAudience::Coordinator);

        assert_eq!(
            ChildReportTool::from_invocation(&ToolInvocation {
                id: Some("r3".to_string()),
                name: "bash".to_string(),
                input: serde_json::json!({}),
            })
            .expect("unknown tool should not fail"),
            None
        );
    }
}
