//! Built-in agent tools for deterministic skill-procedure execution.
//!
//! These tools are the agent-facing counterpart to the `Skills` service
//! procedure lifecycle: `run_procedure` starts a selected skill's procedure and
//! `procedure_status` polls it. They are declared here alongside the delegation
//! tools so the model-facing schema, name, and parsing live in one place, and are
//! injected into a coordinator turn only when a selected skill carries a
//! procedure. Actual execution stays on the workflow-owned path in the
//! orchestrator, mirroring how delegation tools reach Restate services.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::error::{MoaError, Result};

use super::completion::ToolInvocation;

/// Stable kind for one built-in procedure tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcedureToolKind {
    /// Start a skill's procedure run.
    Run,
    /// Poll a previously started procedure run.
    Status,
}

impl ProcedureToolKind {
    /// All built-in procedure tool kinds in stable prompt order.
    pub const ALL: [Self; 2] = [Self::Run, Self::Status];

    /// Returns the stable provider-facing tool name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Run => "run_procedure",
            Self::Status => "procedure_status",
        }
    }

    /// Returns the kind for a provider-facing procedure tool name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Returns the provider-facing JSON schema for this tool.
    #[must_use]
    pub fn schema(self) -> Value {
        match self {
            Self::Run => run_procedure_tool_schema(),
            Self::Status => procedure_status_tool_schema(),
        }
    }
}

/// Payload for the `run_procedure` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunProcedureToolInput {
    /// Skill whose procedure to run, as a name or a `skill://<name>` reference.
    pub skill: String,
    /// Procedure input object satisfying the procedure's `input_schema`.
    #[serde(default = "empty_object_value")]
    pub input: Value,
    /// Optional idempotency key so repeated starts do not create duplicate runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl RunProcedureToolInput {
    /// Normalizes the caller-supplied skill into a canonical `skill://<name>` reference.
    ///
    /// Accepts either a bare skill name or an already-qualified `skill://` reference
    /// so the model can name a skill exactly as it appears in the manifest.
    #[must_use]
    pub fn procedure_ref(&self) -> String {
        normalize_procedure_skill_ref(&self.skill)
    }
}

/// Normalizes a procedure skill identifier into a canonical `skill://<name>` reference.
///
/// This is the single normalization used both when a `run_procedure` call names its
/// target skill and when the turn's selected procedure-capable skill names are turned
/// into a membership set, so a call and the allowlist compare on identical forms. A
/// bare name is prefixed and an already-qualified `skill://` reference is preserved.
#[must_use]
pub fn normalize_procedure_skill_ref(skill: &str) -> String {
    let trimmed = skill.trim();
    if trimmed.starts_with("skill://") {
        trimmed.to_string()
    } else {
        format!("skill://{trimmed}")
    }
}

/// Payload for the `procedure_status` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcedureStatusToolInput {
    /// Run id returned by a previous `run_procedure` call.
    pub run_id: String,
}

/// Parsed payload for one built-in procedure tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcedureTool {
    /// Start a skill's procedure run.
    Run(RunProcedureToolInput),
    /// Poll a previously started procedure run.
    Status(ProcedureStatusToolInput),
}

impl ProcedureTool {
    /// Parses a provider invocation into a typed procedure tool when recognized.
    pub fn from_invocation(invocation: &ToolInvocation) -> Result<Option<Self>> {
        let Some(kind) = ProcedureToolKind::from_name(&invocation.name) else {
            return Ok(None);
        };

        Ok(Some(match kind {
            ProcedureToolKind::Run => Self::Run(parse_procedure_tool_input(invocation)?),
            ProcedureToolKind::Status => Self::Status(parse_procedure_tool_input(invocation)?),
        }))
    }

    /// Returns the parsed tool kind.
    #[must_use]
    pub fn kind(&self) -> ProcedureToolKind {
        match self {
            Self::Run(_) => ProcedureToolKind::Run,
            Self::Status(_) => ProcedureToolKind::Status,
        }
    }

    /// Returns the stable provider-facing tool name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.kind().name()
    }
}

/// Returns whether a provider-facing tool name is a built-in procedure tool.
#[must_use]
pub fn is_procedure_tool_name(name: &str) -> bool {
    ProcedureToolKind::from_name(name).is_some()
}

/// Returns all procedure tool schemas in stable prompt order.
#[must_use]
pub fn procedure_tool_schemas() -> Vec<Value> {
    ProcedureToolKind::ALL
        .into_iter()
        .map(ProcedureToolKind::schema)
        .collect()
}

/// Stable `run_procedure` tool schema.
#[must_use]
pub fn run_procedure_tool_schema() -> Value {
    serde_json::json!({
        "name": "run_procedure",
        "description": "Deterministically execute a selected skill's procedure instead of improvising the steps. Use this when a selected skill is marked [procedure] and the task matches it. Provide the skill and an input object that satisfies the procedure's required inputs. If required inputs are missing, the result reports exactly which fields to collect from the user before retrying, so ask for those fields and call again. The run is durable and may pause for review; use procedure_status with the returned run_id to poll it rather than waiting.",
        "input_schema": {
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "Skill name (or skill://<name>) whose procedure to run. Must be a selected skill marked [procedure] in the skill manifest."
                },
                "input": {
                    "type": "object",
                    "description": "Procedure input object. Include every field the procedure requires; omit unknown fields."
                },
                "idempotency_key": {
                    "type": "string",
                    "description": "Optional key so a retried start reuses the same run instead of creating a duplicate."
                }
            },
            "required": ["skill"],
            "additionalProperties": false
        }
    })
}

/// Stable `procedure_status` tool schema.
#[must_use]
pub fn procedure_status_tool_schema() -> Value {
    serde_json::json!({
        "name": "procedure_status",
        "description": "Poll a previously started procedure run and report its current status, node progress, terminal output, or error. Use the run_id returned by run_procedure.",
        "input_schema": {
            "type": "object",
            "properties": {
                "run_id": {
                    "type": "string",
                    "description": "Run id returned by run_procedure."
                }
            },
            "required": ["run_id"],
            "additionalProperties": false
        }
    })
}

fn parse_procedure_tool_input<T>(invocation: &ToolInvocation) -> Result<T>
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

fn empty_object_value() -> Value {
    Value::Object(serde_json::Map::new())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        ProcedureTool, ProcedureToolKind, RunProcedureToolInput, is_procedure_tool_name,
        procedure_tool_schemas,
    };
    use crate::types::completion::ToolInvocation;

    fn invocation(name: &str, input: serde_json::Value) -> ToolInvocation {
        ToolInvocation {
            id: Some("call-1".to_string()),
            name: name.to_string(),
            input,
        }
    }

    #[test]
    fn run_procedure_parses_skill_and_input_and_defaults_input_to_object() {
        // Pins: run_procedure parses the skill + input, and an omitted input becomes
        // an empty object so a no-required-input procedure starts with just a skill.
        let tool = ProcedureTool::from_invocation(&invocation(
            "run_procedure",
            json!({"skill": "damaged-food-order", "input": {"order_id": "A-1"}}),
        ))
        .expect("parse ok")
        .expect("recognized tool");
        let ProcedureTool::Run(run) = tool else {
            panic!("expected run tool");
        };
        assert_eq!(run.skill, "damaged-food-order");
        assert_eq!(run.input, json!({"order_id": "A-1"}));
        assert_eq!(run.procedure_ref(), "skill://damaged-food-order");

        let defaulted = ProcedureTool::from_invocation(&invocation(
            "run_procedure",
            json!({"skill": "skill://greeter"}),
        ))
        .expect("parse ok")
        .expect("recognized tool");
        let ProcedureTool::Run(RunProcedureToolInput { input, skill, .. }) = defaulted else {
            panic!("expected run tool");
        };
        assert_eq!(input, json!({}));
        // An already-qualified reference is preserved rather than double-prefixed.
        assert_eq!(
            RunProcedureToolInput {
                skill,
                input,
                idempotency_key: None
            }
            .procedure_ref(),
            "skill://greeter"
        );
    }

    #[test]
    fn procedure_status_parses_run_id() {
        // Pins: procedure_status carries the run id the agent polls.
        let tool = ProcedureTool::from_invocation(&invocation(
            "procedure_status",
            json!({"run_id": "018f-run"}),
        ))
        .expect("parse ok")
        .expect("recognized tool");
        let ProcedureTool::Status(status) = tool else {
            panic!("expected status tool");
        };
        assert_eq!(status.run_id, "018f-run");
        assert_eq!(
            tool_kind_name(&ProcedureTool::Status(status)),
            "procedure_status"
        );
    }

    fn tool_kind_name(tool: &ProcedureTool) -> &'static str {
        tool.name()
    }

    #[test]
    fn unknown_tool_name_is_not_a_procedure_tool() {
        // Pins: non-procedure tool names are ignored so the router keeps handling them.
        assert!(
            ProcedureTool::from_invocation(&invocation("spawn_worker", json!({"task": "x"})))
                .expect("parse ok")
                .is_none()
        );
        assert!(!is_procedure_tool_name("spawn_worker"));
        assert!(is_procedure_tool_name("run_procedure"));
        assert!(is_procedure_tool_name("procedure_status"));
    }

    #[test]
    fn malformed_run_procedure_input_is_a_parse_error() {
        // Pins: missing the required `skill` field is a structured parse error, not a
        // silent default, so the caller learns the tool arguments were wrong.
        let error = ProcedureTool::from_invocation(&invocation("run_procedure", json!({})))
            .expect_err("missing skill is an error");
        assert!(error.to_string().contains("run_procedure"));
    }

    #[test]
    fn schemas_cover_both_tools_in_stable_order() {
        // Pins: both procedure tools are offered with stable names and required fields.
        let schemas = procedure_tool_schemas();
        let names = schemas
            .iter()
            .map(|schema| schema["name"].as_str().expect("name").to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["run_procedure", "procedure_status"]);
        assert_eq!(
            schemas[0]["input_schema"]["required"],
            json!(["skill"]),
            "run_procedure requires a skill"
        );
        assert_eq!(ProcedureToolKind::Run.name(), "run_procedure");
        assert_eq!(
            ProcedureToolKind::from_name("procedure_status"),
            Some(ProcedureToolKind::Status)
        );
    }
}
