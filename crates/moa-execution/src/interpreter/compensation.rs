//! Bounded deterministic compensation-input construction.

use std::collections::BTreeSet;

use moa_artifacts::execution_plan::{CompensationInputMapping, CompensationValueSource};
use serde_json::{Map, Value};

use crate::{Error, Result, schema::validate_instance};

/// Resolves an exact compensation input from persisted forward input and output.
///
/// The mapping may only construct an object through RFC 6901 pointers. Missing
/// sources, duplicate targets, and parent/child target collisions fail closed.
pub fn resolve_compensation_input(
    mapping: &CompensationInputMapping,
    forward_input: &Value,
    forward_output: &Value,
    compensator_input_schema: &Value,
) -> Result<Value> {
    let mut targets = BTreeSet::new();
    let mut decoded_targets = Vec::with_capacity(mapping.bindings.len());
    for binding in &mapping.bindings {
        let segments = decode_target_pointer(&binding.target_pointer)?;
        if !targets.insert(segments.clone())
            || decoded_targets.iter().any(|existing: &Vec<String>| {
                is_prefix(existing, &segments) || is_prefix(&segments, existing)
            })
        {
            return Err(Error::InvalidProjection {
                message: format!(
                    "compensation target pointer `{}` collides with another binding",
                    binding.target_pointer
                ),
            });
        }
        decoded_targets.push(segments);
    }

    let mut mapped = Value::Object(Map::new());
    for (binding, segments) in mapping.bindings.iter().zip(decoded_targets) {
        let (source, pointer) = match &binding.source {
            CompensationValueSource::OriginalInput { pointer } => (forward_input, pointer),
            CompensationValueSource::OriginalOutput { pointer } => (forward_output, pointer),
        };
        let value = source
            .pointer(pointer)
            .cloned()
            .ok_or_else(|| Error::Binding {
                path: pointer.clone(),
                message:
                    "compensation source pointer did not resolve against persisted forward values"
                        .to_string(),
            })?;
        insert_target(&mut mapped, &segments, value, &binding.target_pointer)?;
    }
    validate_instance(
        compensator_input_schema,
        &mapped,
        "execution_compensation.mapped_input",
    )?;
    Ok(mapped)
}

fn decode_target_pointer(pointer: &str) -> Result<Vec<String>> {
    let encoded = pointer
        .strip_prefix('/')
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Binding {
            path: pointer.to_string(),
            message: "compensation target must be a non-root JSON Pointer".to_string(),
        })?;
    encoded
        .split('/')
        .map(|segment| decode_pointer_segment(segment, pointer))
        .collect()
}

fn decode_pointer_segment(segment: &str, pointer: &str) -> Result<String> {
    let mut decoded = String::with_capacity(segment.len());
    let mut chars = segment.chars();
    while let Some(character) = chars.next() {
        if character != '~' {
            decoded.push(character);
            continue;
        }
        match chars.next() {
            Some('0') => decoded.push('~'),
            Some('1') => decoded.push('/'),
            _ => {
                return Err(Error::Binding {
                    path: pointer.to_string(),
                    message: "compensation target contains an invalid JSON Pointer escape"
                        .to_string(),
                });
            }
        }
    }
    Ok(decoded)
}

fn is_prefix(left: &[String], right: &[String]) -> bool {
    left.len() < right.len() && right.starts_with(left)
}

fn insert_target(root: &mut Value, segments: &[String], value: Value, pointer: &str) -> Result<()> {
    let Some((leaf, parents)) = segments.split_last() else {
        return Err(Error::Binding {
            path: pointer.to_string(),
            message: "compensation target must name one object field".to_string(),
        });
    };
    let mut current = root;
    for segment in parents {
        let object = current.as_object_mut().ok_or_else(|| Error::Binding {
            path: pointer.to_string(),
            message: "compensation target traverses a non-object value".to_string(),
        })?;
        current = object
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let object = current.as_object_mut().ok_or_else(|| Error::Binding {
        path: pointer.to_string(),
        message: "compensation target parent is not an object".to_string(),
    })?;
    if object.insert(leaf.clone(), value).is_some() {
        return Err(Error::Binding {
            path: pointer.to_string(),
            message: "compensation target was populated more than once".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use moa_artifacts::execution_plan::{
        CompensationInputBinding, CompensationInputMapping, CompensationValueSource,
    };
    use serde_json::json;

    use super::resolve_compensation_input;

    #[test]
    fn compensation_mapping_uses_exact_persisted_values_and_validates_schema() {
        // Pins: compensation input is a bounded object built only from exact persisted values.
        let mapping = CompensationInputMapping {
            bindings: vec![
                CompensationInputBinding {
                    target_pointer: "/resource/id".to_string(),
                    source: CompensationValueSource::OriginalOutput {
                        pointer: "/id".to_string(),
                    },
                },
                CompensationInputBinding {
                    target_pointer: "/request/tenant".to_string(),
                    source: CompensationValueSource::OriginalInput {
                        pointer: "/tenant".to_string(),
                    },
                },
            ],
        };
        let schema = json!({
            "type": "object",
            "required": ["resource", "request"],
            "properties": {
                "resource": {"type":"object", "required":["id"], "properties":{"id":{"type":"string"}}},
                "request": {"type":"object", "required":["tenant"], "properties":{"tenant":{"type":"string"}}}
            }
        });

        let mapped = resolve_compensation_input(
            &mapping,
            &json!({"tenant":"tenant-a"}),
            &json!({"id":"object-7"}),
            &schema,
        )
        .expect("exact compensation mapping should resolve");

        assert_eq!(
            mapped,
            json!({"resource":{"id":"object-7"},"request":{"tenant":"tenant-a"}})
        );
    }

    #[test]
    fn compensation_mapping_rejects_parent_child_target_collisions() {
        // Pins: mapping order cannot change semantics through overlapping targets.
        let mapping = CompensationInputMapping {
            bindings: vec![
                CompensationInputBinding {
                    target_pointer: "/resource".to_string(),
                    source: CompensationValueSource::OriginalInput {
                        pointer: "/resource".to_string(),
                    },
                },
                CompensationInputBinding {
                    target_pointer: "/resource/id".to_string(),
                    source: CompensationValueSource::OriginalOutput {
                        pointer: "/id".to_string(),
                    },
                },
            ],
        };

        let error = resolve_compensation_input(
            &mapping,
            &json!({"resource":{}}),
            &json!({"id":"object-7"}),
            &json!({"type":"object"}),
        )
        .expect_err("overlapping targets must fail closed");

        assert!(error.to_string().contains("collides"));
    }
}
