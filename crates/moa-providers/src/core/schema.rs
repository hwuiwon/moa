//! Provider-specific schema compilation utilities.

use serde_json::{Map, Value, json};

/// Compiles a canonical tool schema into a Gemini-compatible function schema.
pub fn compile_for_gemini(schema: &Value) -> Value {
    let mut compiled = schema.clone();

    if let Some(function) = compiled.get_mut("function").and_then(Value::as_object_mut) {
        if let Some(parameters) = function.get_mut("parameters") {
            make_gemini_compatible(parameters);
        }
        if let Some(input_schema) = function.get_mut("input_schema") {
            make_gemini_compatible(input_schema);
        }
        return compiled;
    }

    if let Some(parameters) = compiled.get_mut("parameters") {
        make_gemini_compatible(parameters);
        return compiled;
    }
    if let Some(input_schema) = compiled.get_mut("input_schema") {
        make_gemini_compatible(input_schema);
        return compiled;
    }

    make_gemini_compatible(&mut compiled);
    compiled
}

/// Compiles a canonical tool schema into an `OpenAI` strict-mode compatible schema.
pub fn compile_for_openai_strict(schema: &Value) -> Value {
    let mut compiled = schema.clone();

    if let Some(function) = compiled.get_mut("function").and_then(Value::as_object_mut) {
        if let Some(parameters) = function.get_mut("parameters") {
            make_strict_compatible(parameters);
        }
        return compiled;
    }

    if let Some(parameters) = compiled.get_mut("parameters") {
        make_strict_compatible(parameters);
        return compiled;
    }
    if let Some(input_schema) = compiled.get_mut("input_schema") {
        make_strict_compatible(input_schema);
        return compiled;
    }

    make_strict_compatible(&mut compiled);
    compiled
}

/// Normalizes one strict OpenAI JSON result back to the canonical schema's
/// omission semantics.
pub fn normalize_openai_strict_output(output: &mut Value, canonical_schema: &Value) {
    let Some(schema) = canonical_schema.as_object() else {
        return;
    };

    match output {
        Value::Object(fields) => {
            let required = schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<std::collections::HashSet<_>>();
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return;
            };
            fields.retain(|name, value| !value.is_null() || required.contains(name.as_str()));
            for (name, value) in fields {
                if let Some(property_schema) = properties.get(name) {
                    normalize_openai_strict_output(value, property_schema);
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for item in items {
                    normalize_openai_strict_output(item, item_schema);
                }
            }
        }
        _ => {}
    }
}

fn make_gemini_compatible(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };

    object.remove("additionalProperties");

    if let Some(items) = object.get_mut("items") {
        make_gemini_compatible(items);
    }
    for key in ["anyOf", "allOf", "oneOf"] {
        if let Some(variants) = object.get_mut(key).and_then(Value::as_array_mut) {
            for variant in variants {
                make_gemini_compatible(variant);
            }
        }
    }
    if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
        for property in properties.values_mut() {
            make_gemini_compatible(property);
        }
    }
}

fn make_strict_compatible(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };

    infer_missing_type(object);
    strip_validation_keywords(object);

    if let Some(items) = object.get_mut("items") {
        make_strict_compatible(items);
    }
    for key in ["anyOf", "allOf", "oneOf"] {
        if let Some(variants) = object.get_mut(key).and_then(Value::as_array_mut) {
            for variant in variants {
                make_strict_compatible(variant);
            }
        }
    }

    let property_names = object
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect::<Vec<_>>());
    let required_names = object
        .get("required")
        .and_then(Value::as_array)
        .map(|required| {
            required
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let allows_object = match object.get("type") {
        Some(Value::String(kind)) => kind == "object",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "object"),
        _ => object.contains_key("properties"),
    };
    if allows_object {
        object.insert("additionalProperties".to_string(), Value::Bool(false));
    }

    if let Some(property_names) = property_names {
        if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
            for property_name in &property_names {
                if let Some(property_schema) = properties.get_mut(property_name) {
                    if !required_names
                        .iter()
                        .any(|required| required == property_name)
                    {
                        make_nullable(property_schema);
                    }
                    make_strict_compatible(property_schema);
                }
            }
        }

        object.insert("required".to_string(), json!(property_names));
    }
}

fn infer_missing_type(object: &mut Map<String, Value>) {
    if object.contains_key("type") {
        return;
    }

    let inferred = object
        .get("const")
        .and_then(json_value_type)
        .or_else(|| homogeneous_enum_type(object.get("enum")));
    if let Some(inferred) = inferred {
        object.insert("type".to_string(), Value::String(inferred.to_string()));
    }
}

fn homogeneous_enum_type(value: Option<&Value>) -> Option<&'static str> {
    let values = value?.as_array()?;
    let first = json_value_type(values.first()?)?;
    values
        .iter()
        .all(|value| json_value_type(value) == Some(first))
        .then_some(first)
}

fn json_value_type(value: &Value) -> Option<&'static str> {
    match value {
        Value::Null => Some("null"),
        Value::Bool(_) => Some("boolean"),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some("integer"),
        Value::Number(_) => Some("number"),
        Value::String(_) => Some("string"),
        Value::Array(_) => Some("array"),
        Value::Object(_) => Some("object"),
    }
}

fn make_nullable(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };

    match object.get_mut("type") {
        Some(Value::String(kind)) => {
            let kind = kind.clone();
            object.insert("type".to_string(), json!([kind, "null"]));
        }
        Some(Value::Array(kinds)) if !kinds.iter().any(|kind| kind == "null") => {
            kinds.push(Value::String("null".to_string()));
        }
        _ => {}
    }
}

fn strip_validation_keywords(object: &mut Map<String, Value>) {
    for keyword in [
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "pattern",
        "minItems",
        "maxItems",
        "minLength",
        "maxLength",
    ] {
        object.remove(keyword);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{compile_for_gemini, compile_for_openai_strict, normalize_openai_strict_output};

    #[test]
    fn compile_for_gemini_removes_additional_properties_recursively() {
        let schema = json!({
            "input_schema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "outer": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "inner": {
                                "type": "string"
                            }
                        }
                    }
                }
            }
        });

        let compiled = compile_for_gemini(&schema);
        let input_schema = &compiled["input_schema"];

        assert!(input_schema.get("additionalProperties").is_none());
        assert!(
            input_schema["properties"]["outer"]
                .get("additionalProperties")
                .is_none()
        );
    }

    #[test]
    fn compile_for_openai_strict_makes_optional_properties_required_and_nullable() {
        let schema = json!({
            "name": "file_search",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "root": { "type": "string" }
                },
                "required": ["pattern"]
            }
        });

        let compiled = compile_for_openai_strict(&schema);
        let input_schema = &compiled["input_schema"];

        assert_eq!(input_schema["required"], json!(["pattern", "root"]));
        assert_eq!(
            input_schema["properties"]["pattern"]["type"],
            json!("string")
        );
        assert_eq!(
            input_schema["properties"]["root"]["type"],
            json!(["string", "null"])
        );
    }

    #[test]
    fn compile_for_openai_strict_adds_additional_properties_false_recursively() {
        let schema = json!({
            "parameters": {
                "type": "object",
                "properties": {
                    "outer": {
                        "type": "object",
                        "properties": {
                            "inner": { "type": "string" }
                        }
                    }
                }
            }
        });

        let compiled = compile_for_openai_strict(&schema);
        let parameters = &compiled["parameters"];

        assert_eq!(parameters["additionalProperties"], json!(false));
        assert_eq!(
            parameters["properties"]["outer"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn compile_for_openai_strict_closes_nullable_objects_without_properties() {
        // Pins: optional builder-owned JSON object fields still satisfy OpenAI's
        // strict requirement that every object schema declares additionalProperties=false.
        let schema = json!({
            "type": "object",
            "properties": {
                "ui": { "type": "object" }
            }
        });

        let compiled = compile_for_openai_strict(&schema);
        let ui = &compiled["properties"]["ui"];

        assert_eq!(ui["type"], json!(["object", "null"]));
        assert_eq!(ui["additionalProperties"], json!(false));
    }

    #[test]
    fn normalize_openai_strict_output_omits_only_canonical_optional_nulls() {
        // Pins: OpenAI's required-and-nullable encoding maps back to omitted
        // serde-default fields without erasing genuinely required nullable values.
        let schema = json!({
            "type": "object",
            "required": ["required_nullable", "nested"],
            "properties": {
                "required_nullable": { "type": ["string", "null"] },
                "defaulted_items": { "type": "array", "items": { "type": "string" } },
                "nested": {
                    "type": "object",
                    "properties": {
                        "defaulted_label": { "type": "string" }
                    }
                }
            }
        });
        let mut output = json!({
            "required_nullable": null,
            "defaulted_items": null,
            "nested": { "defaulted_label": null }
        });

        normalize_openai_strict_output(&mut output, &schema);

        assert_eq!(
            output,
            json!({
                "required_nullable": null,
                "nested": {}
            })
        );
    }

    #[test]
    fn compile_for_openai_strict_strips_validation_only_keywords() {
        let schema = json!({
            "input_schema": {
                "type": "object",
                "properties": {
                    "count": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 10
                    }
                }
            }
        });

        let compiled = compile_for_openai_strict(&schema);
        let count = &compiled["input_schema"]["properties"]["count"];

        assert!(count.get("minimum").is_none());
        assert!(count.get("maximum").is_none());
    }

    #[test]
    fn compile_for_openai_strict_preserves_existing_required_properties() {
        let schema = json!({
            "input_schema": {
                "type": "object",
                "properties": {
                    "cmd": { "type": "string" }
                },
                "required": ["cmd"]
            }
        });

        let compiled = compile_for_openai_strict(&schema);
        assert_eq!(
            compiled["input_schema"]["properties"]["cmd"]["type"],
            json!("string")
        );
    }

    #[test]
    fn compile_for_openai_strict_does_not_duplicate_null_in_type_arrays() {
        let schema = json!({
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": ["string", "null"] }
                }
            }
        });

        let compiled = compile_for_openai_strict(&schema);
        assert_eq!(
            compiled["input_schema"]["properties"]["path"]["type"],
            json!(["string", "null"])
        );
    }

    #[test]
    fn compile_for_openai_strict_infers_const_and_enum_types() {
        // Pins: provider-neutral const/enum schemas gain the explicit JSON types
        // OpenAI strict mode requires without overriding an existing type.
        let schema = json!({
            "type": "object",
            "properties": {
                "api_version": { "const": "moa.artifact/v1" },
                "kind": { "enum": ["experiment_plan", "skill"] },
                "count": { "const": 2 },
                "score": { "enum": [0.5, 1.0] },
                "enabled": { "const": true },
                "items": { "const": [] },
                "metadata": { "const": {} },
                "mixed": { "enum": ["one", 2] },
                "explicit": { "type": "string", "const": "kept" }
            },
            "required": [
                "api_version", "kind", "count", "score", "enabled", "items",
                "metadata", "mixed", "explicit"
            ]
        });

        let compiled = compile_for_openai_strict(&schema);
        let properties = &compiled["properties"];

        assert_eq!(properties["api_version"]["type"], json!("string"));
        assert_eq!(properties["kind"]["type"], json!("string"));
        assert_eq!(properties["count"]["type"], json!("integer"));
        assert_eq!(properties["score"]["type"], json!("number"));
        assert_eq!(properties["enabled"]["type"], json!("boolean"));
        assert_eq!(properties["items"]["type"], json!("array"));
        assert_eq!(properties["metadata"]["type"], json!("object"));
        assert!(properties["mixed"].get("type").is_none());
        assert_eq!(properties["explicit"]["type"], json!("string"));
    }
}
