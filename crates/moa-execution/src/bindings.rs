//! Restricted whole-value execution binding and stable map-key resolution.

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::execution_plan::ExecutionCondition;
use moa_core::canonical_json::canonical_json_bytes;
use serde_json::Value;

use crate::{Error, Result};

/// Runtime values visible while resolving one plan node or map task input.
#[derive(Clone, Copy)]
pub struct BindingContext<'a> {
    /// Immutable run input.
    pub run_input: &'a Value,
    /// Completed dependency outputs keyed by node ID.
    pub node_outputs: &'a BTreeMap<String, Value>,
    /// Direct dependency IDs visible to this node.
    pub dependencies: &'a BTreeSet<String>,
    /// Current map item, when resolving a map task input.
    pub item: Option<&'a Value>,
    /// Current encoded map item key, when resolving a map task input.
    pub item_key: Option<&'a str>,
}

/// Resolves only whole-value `$ref`, `$item`, and `$item_key` binding objects.
pub fn resolve_bindings(value: &Value, context: &BindingContext<'_>) -> Result<Value> {
    resolve_value(value, context, "$input")
}

/// Evaluates an execution condition against run input and direct dependencies.
///
/// Both forms compare whole values: an absent reference is false rather than an
/// error, so a condition may name an optional field without the plan failing.
pub fn evaluate_condition(
    condition: &ExecutionCondition,
    context: &BindingContext<'_>,
) -> Result<bool> {
    match condition {
        ExecutionCondition::Exists { reference } => {
            Ok(try_resolve_reference(&reference.path, context)?.is_some())
        }
        ExecutionCondition::Equals { reference, value } => {
            Ok(try_resolve_reference(&reference.path, context)?.as_ref() == Some(value))
        }
    }
}

/// Resolves one restricted execution reference.
pub fn resolve_reference(path: &str, context: &BindingContext<'_>) -> Result<Value> {
    try_resolve_reference(path, context)?.ok_or_else(|| Error::Binding {
        path: path.to_string(),
        message: "referenced value does not exist".to_string(),
    })
}

/// Extracts and canonically encodes one map item key through an RFC 6901 pointer.
pub fn extract_map_key(item: &Value, item_key_pointer: &str) -> Result<String> {
    let value = if item_key_pointer.is_empty() {
        item
    } else {
        item.pointer(item_key_pointer)
            .ok_or_else(|| Error::Binding {
                path: item_key_pointer.to_string(),
                message: "map item_key pointer did not resolve".to_string(),
            })?
    };
    encode_map_key(value)
}

/// Encodes any JSON value into the typed canonical map-key format.
pub fn encode_map_key(value: &Value) -> Result<String> {
    let (prefix, suffix) = match value {
        Value::Null => ("null:", String::new()),
        Value::Bool(_) => ("bool:", canonical_json_string(value)?),
        Value::Number(_) => ("number:", canonical_json_string(value)?),
        Value::String(_) => ("string:", canonical_json_string(value)?),
        Value::Array(_) => ("array:", canonical_json_string(value)?),
        Value::Object(_) => ("object:", canonical_json_string(value)?),
    };
    let key = format!("{prefix}{suffix}");
    if key.len() > 1_024 {
        return Err(Error::Binding {
            path: "$item_key".to_string(),
            message: "encoded map item key exceeds 1,024 UTF-8 bytes".to_string(),
        });
    }
    Ok(key)
}

fn resolve_value(value: &Value, context: &BindingContext<'_>, path: &str) -> Result<Value> {
    match value {
        Value::Array(values) => values
            .iter()
            .enumerate()
            .map(|(index, value)| resolve_value(value, context, &format!("{path}[{index}]")))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        Value::Object(object) => {
            let dynamic_keys = ["$ref", "$item", "$item_key"]
                .into_iter()
                .filter(|key| object.contains_key(*key))
                .collect::<Vec<_>>();
            if !dynamic_keys.is_empty() {
                if dynamic_keys.len() != 1 || object.len() != 1 {
                    return Err(Error::Binding {
                        path: path.to_string(),
                        message: "dynamic binding object must contain exactly one supported key"
                            .to_string(),
                    });
                }
                let key = dynamic_keys[0];
                let binding = object.get(key).ok_or_else(|| Error::Binding {
                    path: path.to_string(),
                    message: "dynamic binding key disappeared during resolution".to_string(),
                })?;
                return match key {
                    "$ref" => {
                        let reference = binding.as_str().ok_or_else(|| Error::Binding {
                            path: path.to_string(),
                            message: "$ref binding must contain a string".to_string(),
                        })?;
                        resolve_reference(reference, context)
                    }
                    "$item" => {
                        require_true(binding, path)?;
                        context.item.cloned().ok_or_else(|| Error::Binding {
                            path: path.to_string(),
                            message: "$item is only available while resolving a map task"
                                .to_string(),
                        })
                    }
                    "$item_key" => {
                        require_true(binding, path)?;
                        context
                            .item_key
                            .map(|key| Value::String(key.to_string()))
                            .ok_or_else(|| Error::Binding {
                                path: path.to_string(),
                                message: "$item_key is only available while resolving a map task"
                                    .to_string(),
                            })
                    }
                    _ => Err(Error::Binding {
                        path: path.to_string(),
                        message: "unsupported dynamic binding".to_string(),
                    }),
                };
            }
            if object.keys().any(|key| key.starts_with('$')) {
                return Err(Error::Binding {
                    path: path.to_string(),
                    message: "unsupported dynamic binding key".to_string(),
                });
            }

            let mut resolved = serde_json::Map::new();
            for (key, value) in object {
                resolved.insert(
                    key.clone(),
                    resolve_value(value, context, &format!("{path}.{key}"))?,
                );
            }
            Ok(Value::Object(resolved))
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(value.clone()),
    }
}

fn try_resolve_reference(path: &str, context: &BindingContext<'_>) -> Result<Option<Value>> {
    if let Some(tail) = path.strip_prefix("$.input") {
        return lookup_tail(context.run_input, tail, path).map(|value| value.cloned());
    }

    let rest = path
        .strip_prefix("$.nodes.")
        .ok_or_else(|| Error::Binding {
            path: path.to_string(),
            message: "reference must target $.input or $.nodes.<id>.output".to_string(),
        })?;
    let (node_id, tail) = rest.split_once(".output").ok_or_else(|| Error::Binding {
        path: path.to_string(),
        message: "node reference must include .output".to_string(),
    })?;
    if !context.dependencies.contains(node_id) {
        return Err(Error::Binding {
            path: path.to_string(),
            message: "reference may only read a declared dependency output".to_string(),
        });
    }
    let Some(output) = context.node_outputs.get(node_id) else {
        return Ok(None);
    };
    lookup_tail(output, tail, path).map(|value| value.cloned())
}

fn lookup_tail<'a>(value: &'a Value, tail: &str, path: &str) -> Result<Option<&'a Value>> {
    if tail.is_empty() {
        return Ok(Some(value));
    }
    let fields = tail.strip_prefix('.').ok_or_else(|| Error::Binding {
        path: path.to_string(),
        message: "reference fields must use dot-separated segments".to_string(),
    })?;
    if fields.is_empty() {
        return Err(Error::Binding {
            path: path.to_string(),
            message: "reference field segment must not be empty".to_string(),
        });
    }
    let mut current = value;
    for field in fields.split('.') {
        if !valid_reference_segment(field) {
            return Err(Error::Binding {
                path: path.to_string(),
                message: "reference contains an invalid field segment".to_string(),
            });
        }
        let Some(next) = current.as_object().and_then(|object| object.get(field)) else {
            return Ok(None);
        };
        current = next;
    }
    Ok(Some(current))
}

fn valid_reference_segment(segment: &str) -> bool {
    let mut characters = segment.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn require_true(value: &Value, path: &str) -> Result<()> {
    if value != &Value::Bool(true) {
        return Err(Error::Binding {
            path: path.to_string(),
            message: "map-variable binding value must be true".to_string(),
        });
    }
    Ok(())
}

fn canonical_json_string(value: &Value) -> Result<String> {
    let bytes = canonical_json_bytes(value)?;
    String::from_utf8(bytes).map_err(|error| Error::Binding {
        path: "$item_key".to_string(),
        message: format!("canonical JSON was not UTF-8: {error}"),
    })
}
