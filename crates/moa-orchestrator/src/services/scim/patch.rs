//! Minimal RFC 7644 PATCH parser for common enterprise IdP operations.

use serde::Deserialize;

/// SCIM PatchOp request.
#[derive(Debug, Deserialize)]
pub struct PatchOp {
    /// Resource schema URNs.
    pub schemas: Vec<String>,
    /// Patch operations.
    #[serde(rename = "Operations")]
    pub operations: Vec<Operation>,
}

/// One SCIM PATCH operation.
#[derive(Debug, Deserialize)]
pub struct Operation {
    /// Operation name: `add`, `remove`, or `replace`.
    pub op: String,
    /// Target path.
    pub path: Option<String>,
    /// Operation value.
    pub value: Option<serde_json::Value>,
}

/// User mutation interpreted from a SCIM patch.
#[derive(Debug, Default)]
pub struct UserMutation {
    /// Active flag mutation.
    pub active: Option<bool>,
    /// Email mutation.
    pub email: Option<String>,
    /// Display name mutation.
    pub display_name: Option<String>,
    /// Given name mutation.
    pub given_name: Option<String>,
    /// Family name mutation.
    pub family_name: Option<String>,
}

/// Group membership mutation interpreted from a SCIM patch.
#[derive(Debug, Default)]
pub struct GroupMutation {
    /// Members to add.
    pub add_members: Vec<String>,
    /// Members to remove.
    pub remove_members: Vec<String>,
    /// Replacement display name.
    pub display_name: Option<String>,
}

/// Interpret a user PATCH body into a compact mutation.
pub fn interpret_user(patch: &PatchOp) -> Result<UserMutation, &'static str> {
    let mut mutation = UserMutation::default();
    for op in &patch.operations {
        let operation = op.op.to_ascii_lowercase();
        let path = op.path.as_deref().unwrap_or("");
        match (operation.as_str(), path) {
            ("replace" | "add", "active") => {
                mutation.active = op.value.as_ref().and_then(serde_json::Value::as_bool);
                if mutation.active.is_none() {
                    return Err("active must be boolean");
                }
            }
            ("replace" | "add", "userName") | ("replace" | "add", "emails") => {
                mutation.email = email_from_value(op.value.as_ref());
            }
            ("replace" | "add", "displayName") => {
                mutation.display_name = string_from_value(op.value.as_ref());
            }
            ("replace" | "add", "name.givenName") => {
                mutation.given_name = string_from_value(op.value.as_ref());
            }
            ("replace" | "add", "name.familyName") => {
                mutation.family_name = string_from_value(op.value.as_ref());
            }
            ("replace" | "add", "") => {
                if let Some(object) = op.value.as_ref().and_then(serde_json::Value::as_object) {
                    if let Some(active) = object.get("active").and_then(serde_json::Value::as_bool)
                    {
                        mutation.active = Some(active);
                    }
                    if let Some(email) = object.get("userName").and_then(serde_json::Value::as_str)
                    {
                        mutation.email = Some(email.to_string());
                    }
                    if let Some(email) = object.get("emails").and_then(email_from_json) {
                        mutation.email = Some(email);
                    }
                    if let Some(value) = object
                        .get("displayName")
                        .and_then(serde_json::Value::as_str)
                    {
                        mutation.display_name = Some(value.to_string());
                    }
                    if let Some(name) = object.get("name").and_then(serde_json::Value::as_object) {
                        if let Some(value) =
                            name.get("givenName").and_then(serde_json::Value::as_str)
                        {
                            mutation.given_name = Some(value.to_string());
                        }
                        if let Some(value) =
                            name.get("familyName").and_then(serde_json::Value::as_str)
                        {
                            mutation.family_name = Some(value.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(mutation)
}

/// Interpret a group PATCH body into membership changes.
pub fn interpret_group(patch: &PatchOp) -> GroupMutation {
    let mut mutation = GroupMutation::default();
    for op in &patch.operations {
        let operation = op.op.to_ascii_lowercase();
        let path = op.path.as_deref().unwrap_or("");
        match (operation.as_str(), path) {
            ("add", "members") => {
                mutation
                    .add_members
                    .extend(member_values(op.value.as_ref()));
            }
            ("remove", "members") => {
                mutation
                    .remove_members
                    .extend(member_values(op.value.as_ref()));
            }
            ("replace" | "add", "displayName") => {
                mutation.display_name = string_from_value(op.value.as_ref());
            }
            ("replace", "") => {
                if let Some(object) = op.value.as_ref().and_then(serde_json::Value::as_object) {
                    if let Some(value) = object
                        .get("displayName")
                        .and_then(serde_json::Value::as_str)
                    {
                        mutation.display_name = Some(value.to_string());
                    }
                    if let Some(members) = object.get("members") {
                        mutation.add_members.extend(member_values(Some(members)));
                    }
                }
            }
            _ if operation == "remove" && path.starts_with("members") => {
                mutation
                    .remove_members
                    .extend(member_values(op.value.as_ref()));
            }
            _ => {}
        }
    }
    mutation
}

fn string_from_value(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}

fn email_from_value(value: Option<&serde_json::Value>) -> Option<String> {
    value.and_then(email_from_json)
}

fn email_from_json(value: &serde_json::Value) -> Option<String> {
    if let Some(email) = value.as_str() {
        return Some(email.to_string());
    }
    if let Some(array) = value.as_array() {
        return array
            .iter()
            .find_map(|item| item.get("value").and_then(serde_json::Value::as_str))
            .map(ToString::to_string);
    }
    value
        .get("value")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}

fn member_values(value: Option<&serde_json::Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    if let Some(array) = value.as_array() {
        return array
            .iter()
            .filter_map(member_value)
            .map(ToString::to_string)
            .collect();
    }
    member_value(value)
        .map(|value| vec![value.to_string()])
        .unwrap_or_default()
}

fn member_value(value: &serde_json::Value) -> Option<&str> {
    value
        .get("value")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.as_str())
}
