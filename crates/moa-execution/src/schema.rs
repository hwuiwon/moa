//! Draft 2020-12 JSON Schema validation with external retrieval disabled.

use std::error::Error as StdError;

use jsonschema::{Draft, Retrieve, Uri};
use serde_json::Value;

use crate::{Error, Result};

/// Validates a JSON Schema object under Draft 2020-12.
///
/// Only local `#` references are accepted; remote and file retrieval are
/// rejected before the validator is built and by the configured retriever.
pub fn validate_schema(schema: &Value, path: &str) -> Result<()> {
    if !schema.is_object() {
        return Err(Error::Schema {
            path: path.to_string(),
            message: "schema must be a JSON object".to_string(),
        });
    }
    reject_external_references(schema, path)?;
    build_validator(schema, path).map(|_| ())
}

/// Validates one JSON instance against a Draft 2020-12 schema.
pub fn validate_instance(schema: &Value, instance: &Value, path: &str) -> Result<()> {
    let validator = build_validator_checked(schema, path)?;
    if let Some(error) = validator.iter_errors(instance).next() {
        return Err(Error::Schema {
            path: format!("{path}{}", error.instance_path()),
            message: error.to_string(),
        });
    }
    Ok(())
}

fn build_validator_checked(schema: &Value, path: &str) -> Result<jsonschema::Validator> {
    if !schema.is_object() {
        return Err(Error::Schema {
            path: path.to_string(),
            message: "schema must be a JSON object".to_string(),
        });
    }
    reject_external_references(schema, path)?;
    build_validator(schema, path)
}

fn build_validator(schema: &Value, path: &str) -> Result<jsonschema::Validator> {
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_retriever(RejectExternalRetriever)
        .build(schema)
        .map_err(|error| Error::Schema {
            path: path.to_string(),
            message: error.to_string(),
        })
}

fn reject_external_references(value: &Value, path: &str) -> Result<()> {
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                reject_external_references(value, &format!("{path}/{index}"))?;
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                let child_path = format!("{path}/{key}");
                if matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef") {
                    let reference = value.as_str().ok_or_else(|| Error::Schema {
                        path: child_path.clone(),
                        message: "schema reference must be a string".to_string(),
                    })?;
                    if !reference.starts_with('#') {
                        return Err(Error::Schema {
                            path: child_path,
                            message: "only local # schema references are allowed".to_string(),
                        });
                    }
                }
                reject_external_references(value, &child_path)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    Ok(())
}

#[derive(Debug)]
struct RejectExternalRetriever;

impl Retrieve for RejectExternalRetriever {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> std::result::Result<Value, Box<dyn StdError + Send + Sync>> {
        Err(format!("external schema retrieval is disabled: {uri}").into())
    }
}
