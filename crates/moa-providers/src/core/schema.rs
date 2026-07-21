//! Provider-specific schema compilation utilities.

use serde_json::{Map, Value, json};

/// Compiles a canonical response schema for Anthropic structured output.
///
/// Anthropic rejects `oneOf` and numeric validation constraints in the schema
/// sent through `output_config.format`, so the provider receives a structural
/// clone with `anyOf` alternatives and without those constraints. The canonical
/// schema remains available to validate the parsed response downstream.
pub fn compile_for_anthropic_output(schema: &Value) -> Value {
    let mut compiled = schema.clone();
    make_anthropic_output_compatible(&mut compiled);
    compiled
}

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

fn make_anthropic_output_compatible(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };

    for keyword in [
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
    ] {
        object.remove(keyword);
    }

    for key in ["properties", "$defs", "definitions"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_object_mut) {
            for child in children.values_mut() {
                make_anthropic_output_compatible(child);
            }
        }
    }
    if let Some(items) = object.get_mut("items") {
        make_anthropic_output_compatible(items);
    }
    for key in ["anyOf", "allOf", "oneOf"] {
        if let Some(variants) = object.get_mut(key).and_then(Value::as_array_mut) {
            for variant in variants {
                make_anthropic_output_compatible(variant);
            }
        }
    }
    rewrite_one_of_as_any_of(object);
}

fn rewrite_one_of_as_any_of(object: &mut Map<String, Value>) {
    if let Some(one_of) = object.remove("oneOf") {
        match object.get_mut("anyOf").and_then(Value::as_array_mut) {
            Some(existing) => {
                if let Value::Array(variants) = one_of {
                    existing.extend(variants);
                }
            }
            None => {
                object.insert("anyOf".to_string(), one_of);
            }
        }
    }
}

/// Reports every OpenAI strict-mode violation in a compiled schema.
///
/// Walks the schema and returns one `path: problem` line per violation of the
/// constraints OpenAI enforces for strict structured output and function
/// parameters: no `oneOf`, `$ref` only without sibling keywords (definition
/// containers excepted), and an explicit `type` on every schema whose shape is
/// not defined by a reference or combinator. Intended for tests that pin a
/// production schema as strict-compatible before it reaches a live provider.
pub fn openai_strict_violations(schema: &Value) -> Vec<String> {
    let mut violations = Vec::new();
    collect_strict_violations(schema, "#", &mut violations);
    violations
}

fn collect_strict_violations(schema: &Value, path: &str, violations: &mut Vec<String>) {
    let Some(object) = schema.as_object() else {
        return;
    };

    if object.contains_key("oneOf") {
        violations.push(format!("{path}: oneOf is not permitted"));
    }
    if object.contains_key("$ref") {
        let extra: Vec<&str> = object
            .keys()
            .filter(|key| !matches!(key.as_str(), "$ref" | "$defs" | "definitions"))
            .map(String::as_str)
            .collect();
        if !extra.is_empty() {
            violations.push(format!("{path}: $ref has sibling keywords {extra:?}"));
        }
    } else if !object.contains_key("type")
        && !["anyOf", "oneOf", "allOf", "enum", "const"]
            .iter()
            .any(|key| object.contains_key(*key))
    {
        violations.push(format!("{path}: schema has no type key"));
    }

    for key in ["properties", "$defs", "definitions"] {
        if let Some(children) = object.get(key).and_then(Value::as_object) {
            for (name, child) in children {
                collect_strict_violations(child, &format!("{path}/{key}/{name}"), violations);
            }
        }
    }
    if let Some(items) = object.get("items") {
        collect_strict_violations(items, &format!("{path}/items"), violations);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = object.get(key).and_then(Value::as_array) {
            for (index, variant) in variants.iter().enumerate() {
                collect_strict_violations(variant, &format!("{path}/{key}/{index}"), violations);
            }
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
    for key in ["$defs", "definitions"] {
        if let Some(definitions) = object.get_mut(key).and_then(Value::as_object_mut) {
            for definition in definitions.values_mut() {
                make_strict_compatible(definition);
            }
        }
    }
    // OpenAI strict mode requires `$ref` to appear without sibling keywords
    // (schemars emits field doc comments as `description` siblings). Definition
    // containers must survive: a root schema may carry `$ref` alongside `$defs`.
    if object.contains_key("$ref") {
        object.retain(|key, _| matches!(key.as_str(), "$ref" | "$defs" | "definitions"));
    }
    // OpenAI strict mode rejects `oneOf`; the equivalent `anyOf` is permitted.
    rewrite_one_of_as_any_of(object);

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
        .or_else(|| homogeneous_enum_type(object.get("enum")))
        .or_else(|| object.contains_key("properties").then_some("object"))
        .or_else(|| object.contains_key("items").then_some("array"));
    if let Some(inferred) = inferred {
        object.insert("type".to_string(), Value::String(inferred.to_string()));
        return;
    }

    // A schema whose shape is defined by a reference or combinator must not be
    // stamped with a type; anything else is an accept-any-value schema, which
    // OpenAI strict mode rejects without an explicit `type` key.
    let shape_defined_elsewhere = ["$ref", "anyOf", "oneOf", "allOf", "enum", "const"]
        .iter()
        .any(|key| object.contains_key(*key));
    if !shape_defined_elsewhere {
        object.insert(
            "type".to_string(),
            json!(["string", "number", "boolean", "null"]),
        );
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

    use super::{
        compile_for_anthropic_output, compile_for_gemini, compile_for_openai_strict,
        normalize_openai_strict_output,
    };

    #[test]
    fn compile_for_anthropic_output_removes_unsupported_nested_schema_features() {
        // Pins: Anthropic rejects `oneOf` and numeric validation constraints in
        // structured output schemas, but accepts their `anyOf` equivalent and
        // the surrounding JSON Schema shape.
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "scores": {
                    "type": "array",
                    "items": {
                        "oneOf": [
                            {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 10,
                                "multipleOf": 1
                            },
                            {
                                "type": "number",
                                "exclusiveMinimum": 0,
                                "exclusiveMaximum": 100
                            }
                        ]
                    }
                }
            },
            "required": ["scores"]
        });

        let compiled = compile_for_anthropic_output(&schema);

        assert_eq!(
            compiled,
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "scores": {
                        "type": "array",
                        "items": {
                            "anyOf": [
                                { "type": "integer" },
                                { "type": "number" }
                            ]
                        }
                    }
                },
                "required": ["scores"]
            })
        );
        assert_eq!(
            schema["properties"]["scores"]["items"]["oneOf"][0]["minimum"],
            1
        );
    }

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
    fn compile_for_openai_strict_rewrites_one_of_to_any_of_including_defs() {
        // Pins: OpenAI strict mode rejects `oneOf` (schemars emits it for
        // documented enums, e.g. the execution-route classifier label); the
        // compiler must rewrite it to the permitted `anyOf`, including inside
        // `$defs` entries referenced from properties.
        let schema = json!({
            "type": "object",
            "properties": {
                "label": { "$ref": "#/$defs/Label" }
            },
            "required": ["label"],
            "$defs": {
                "Label": {
                    "oneOf": [
                        { "const": "respond", "description": "reply" },
                        { "const": "execute", "description": "act" }
                    ]
                }
            }
        });

        let compiled = compile_for_openai_strict(&schema);
        let label = &compiled["$defs"]["Label"];

        assert!(label.get("oneOf").is_none());
        let variants = label["anyOf"].as_array().expect("anyOf variants");
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0]["type"], "string");
    }

    #[test]
    fn compile_for_openai_strict_strips_ref_sibling_keywords() {
        // Pins: OpenAI strict mode rejects `$ref` with sibling keywords such as
        // the `description` schemars emits from field doc comments; definition
        // containers on the same object must survive the strip.
        let schema = json!({
            "type": "object",
            "properties": {
                "label": { "$ref": "#/$defs/Label", "description": "doc comment" }
            },
            "required": ["label"],
            "$defs": {
                "Label": { "type": "string", "enum": ["respond", "execute"] }
            }
        });

        let compiled = compile_for_openai_strict(&schema);
        let label = compiled["properties"]["label"]
            .as_object()
            .expect("label property");

        assert_eq!(label.len(), 1);
        assert!(label.contains_key("$ref"));
        assert!(compiled["$defs"]["Label"].is_object());
    }

    #[test]
    fn compile_for_openai_strict_types_accept_any_value_properties() {
        // Pins: a property with no type/combinator (serde_json::Value fields
        // such as durable-upgrade evidence values) must gain an explicit type
        // key, or OpenAI strict mode rejects the whole tool schema.
        let schema = json!({
            "type": "object",
            "properties": {
                "value": { "description": "arbitrary evidence" }
            },
            "required": ["value"]
        });

        let compiled = compile_for_openai_strict(&schema);
        let value_type = compiled["properties"]["value"]["type"]
            .as_array()
            .expect("type union for any-value property");

        assert!(value_type.iter().any(|kind| kind == "string"));
        assert!(value_type.iter().any(|kind| kind == "null"));
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
