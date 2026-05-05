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

    if let Some(property_names) = property_names {
        object.insert("additionalProperties".to_string(), Value::Bool(false));

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

    use super::{compile_for_gemini, compile_for_openai_strict};

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
}
