//! Focused JSON Schema validation for procedure run input.
//!
//! Starting a procedure must fail deterministically when the caller's input does
//! not satisfy the procedure's declared `input_schema`, so every caller (a human
//! through the API or the agent through the `run_procedure` tool) is forced to
//! collect the required information before execution begins. MOA does not carry a
//! general JSON-Schema validation dependency, so this module implements the
//! narrow subset that procedure input schemas rely on: the top-level `required`
//! array and per-property `type` constraints.

use serde_json::Value;

/// Fields that failed procedure input-schema validation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InputSchemaViolations {
    /// Required fields that were absent from the supplied input.
    pub(crate) missing: Vec<String>,
    /// Provided fields whose value type did not match the declared schema.
    pub(crate) invalid: Vec<String>,
}

impl InputSchemaViolations {
    /// Returns true when the supplied input satisfied the schema.
    pub(crate) fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.invalid.is_empty()
    }
}

/// Validates `input` against the subset of JSON Schema that procedures rely on.
///
/// Checks the top-level `required` array (reporting absent fields as `missing`)
/// and, for each provided field named in `properties`, that the value matches the
/// declared `type` (`object`, `array`, `string`, `number`, `integer`, `boolean`,
/// or `null`), reporting mismatches as `invalid`. A schema that is not a JSON
/// object carries no constraints and is treated as a no-op: artifact validation
/// (`validate_procedure`) does not assert `input_schema` shape at publish time,
/// so run-time validation stays consistent and never rejects an
/// otherwise-published procedure on schema shape alone.
pub(crate) fn validate_input_against_schema(
    schema: &Value,
    input: &Value,
) -> InputSchemaViolations {
    let mut violations = InputSchemaViolations::default();

    let Some(schema_obj) = schema.as_object() else {
        return violations;
    };

    let input_obj = input.as_object();

    if let Some(required) = schema_obj.get("required").and_then(Value::as_array) {
        for field in required.iter().filter_map(Value::as_str) {
            let present = input_obj.is_some_and(|obj| obj.contains_key(field));
            if !present {
                violations.missing.push(field.to_string());
            }
        }
    }

    if let (Some(properties), Some(input_obj)) = (
        schema_obj.get("properties").and_then(Value::as_object),
        input_obj,
    ) {
        for (field, property_schema) in properties {
            let Some(value) = input_obj.get(field) else {
                continue;
            };
            let Some(expected_type) = property_schema.get("type") else {
                continue;
            };
            if !value_matches_type(value, expected_type) {
                violations.invalid.push(field.clone());
            }
        }
    }

    violations.missing.sort();
    violations.missing.dedup();
    violations.invalid.sort();
    violations.invalid.dedup();
    violations
}

/// Returns whether `value` satisfies a JSON-Schema `type` declaration.
///
/// A `type` may be a single type name or an array of accepted type names; any
/// other shape is treated as an unconstrained declaration.
fn value_matches_type(value: &Value, expected_type: &Value) -> bool {
    match expected_type {
        Value::String(type_name) => value_matches_type_name(value, type_name),
        Value::Array(type_names) => type_names
            .iter()
            .filter_map(Value::as_str)
            .any(|type_name| value_matches_type_name(value, type_name)),
        _ => true,
    }
}

/// Returns whether `value` matches a single JSON-Schema primitive type name.
fn value_matches_type_name(value: &Value, type_name: &str) -> bool {
    match type_name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64()
                || value.is_u64()
                || value.as_f64().is_some_and(|number| number.fract() == 0.0)
        }
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::validate_input_against_schema;

    #[test]
    fn missing_required_fields_are_reported_exactly() {
        // Pins: required fields absent from the input are listed as missing so the
        // caller knows exactly which fields to collect before starting the run.
        let schema = json!({
            "type": "object",
            "required": ["order_id", "reason"],
            "properties": {
                "order_id": {"type": "string"},
                "reason": {"type": "string"}
            }
        });
        let input = json!({"order_id": "A-1"});

        let violations = validate_input_against_schema(&schema, &input);

        assert_eq!(violations.missing, vec!["reason".to_string()]);
        assert!(violations.invalid.is_empty());
        assert!(!violations.is_empty());
    }

    #[test]
    fn wrong_type_fields_are_reported_as_invalid() {
        // Pins: a provided field whose value type does not match the schema is
        // reported, covering each primitive JSON type.
        let schema = json!({
            "type": "object",
            "properties": {
                "count": {"type": "integer"},
                "label": {"type": "string"},
                "flags": {"type": "array"},
                "enabled": {"type": "boolean"}
            }
        });
        let input = json!({
            "count": "not-a-number",
            "label": 5,
            "flags": {"nested": true},
            "enabled": true
        });

        let violations = validate_input_against_schema(&schema, &input);

        assert!(violations.missing.is_empty());
        assert_eq!(
            violations.invalid,
            vec![
                "count".to_string(),
                "flags".to_string(),
                "label".to_string()
            ]
        );
    }

    #[test]
    fn valid_input_passes_including_whole_number_integers() {
        // Pins: input satisfying required + type constraints yields no violations,
        // and whole-number JSON values satisfy an `integer` constraint.
        let schema = json!({
            "type": "object",
            "required": ["order_id"],
            "properties": {
                "order_id": {"type": "string"},
                "quantity": {"type": "integer"},
                "meta": {"type": "object"}
            }
        });
        let input = json!({
            "order_id": "A-1",
            "quantity": 3.0,
            "meta": {"source": "chat"}
        });

        let violations = validate_input_against_schema(&schema, &input);

        assert!(
            violations.is_empty(),
            "unexpected violations: {violations:?}"
        );
    }

    #[test]
    fn non_object_schema_is_a_no_op() {
        // Pins: a schema that is not a JSON object carries no constraints, matching
        // artifact validation which does not assert input_schema shape.
        let violations = validate_input_against_schema(&json!(true), &json!({}));
        assert!(violations.is_empty());

        let empty_schema = validate_input_against_schema(&json!({}), &json!("anything"));
        assert!(empty_schema.is_empty());
    }

    #[test]
    fn required_fields_with_non_object_input_are_all_missing() {
        // Pins: when a schema requires fields but the input is not an object, every
        // required field is reported missing rather than silently accepted.
        let schema = json!({
            "type": "object",
            "required": ["order_id", "reason"]
        });

        let violations = validate_input_against_schema(&schema, &json!("scalar"));

        assert_eq!(
            violations.missing,
            vec!["order_id".to_string(), "reason".to_string()]
        );
    }

    #[test]
    fn type_declared_as_array_of_names_accepts_any_listed_type() {
        // Pins: JSON Schema unions (`type: [..]`) accept a value matching any member.
        let schema = json!({
            "type": "object",
            "properties": {"value": {"type": ["string", "null"]}}
        });

        assert!(validate_input_against_schema(&schema, &json!({"value": "x"})).is_empty());
        assert!(validate_input_against_schema(&schema, &json!({"value": null})).is_empty());
        assert_eq!(
            validate_input_against_schema(&schema, &json!({"value": 5})).invalid,
            vec!["value".to_string()]
        );
    }
}
