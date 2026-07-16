//! Deterministic detection of which selected skills a tool call actually engaged.
//!
//! Skill *activation* records every skill the injector placed in a turn manifest.
//! Skill *use* is the narrower signal that the model actually engaged a skill:
//! it read the skill's materialized package under `.moa/skills/<slug>/`, invoked
//! a `skill://<name>` action reference, or passed a selected skill reference to a
//! governed capability. Detection is pure string matching over a tool call's input
//! so it is cheap and replay-stable; there is no LLM or heuristic scoring.

use serde_json::Value;

/// Returns the subset of `selected_skills` that this tool call engaged.
///
/// A skill counts as engaged when the tool call references its materialized
/// package path (`.moa/skills/<slug>/`) or a `skill://` reference to it. Returned
/// names are the exact strings from `selected_skills` so recorded uses line up with the
/// activation names on the same segment. The result preserves `selected_skills`
/// order and contains no duplicates.
#[must_use]
pub fn skills_used_in_tool_call(
    _tool_name: &str,
    input: &Value,
    selected_skills: &[String],
) -> Vec<String> {
    if selected_skills.is_empty() {
        return Vec::new();
    }

    // Collect every string value in the input once and lowercase it so path and
    // `skill://` matching is case-insensitive and order-independent.
    let mut haystack = Vec::new();
    collect_lowercased_strings(input, &mut haystack);

    let mut used = Vec::new();
    for skill in selected_skills {
        let trimmed = skill.trim();
        if trimmed.is_empty() {
            continue;
        }
        let slug = skill_use_slug(trimmed);
        let path_needle = format!(".moa/skills/{slug}/");
        let name_ref = canonical_skill_ref(trimmed).to_ascii_lowercase();
        let slug_ref = format!("skill://{slug}");

        let engaged_by_string = haystack.iter().any(|value| {
            value.contains(&path_needle) || value.contains(&name_ref) || value.contains(&slug_ref)
        });

        if engaged_by_string && !used.iter().any(|name| name == skill) {
            used.push(skill.clone());
        }
    }
    used
}

fn canonical_skill_ref(skill: &str) -> String {
    let trimmed = skill.trim();
    if trimmed.starts_with("skill://") {
        trimmed.to_string()
    } else {
        format!("skill://{trimmed}")
    }
}

/// Converts a skill name into the slug used for its materialized package path.
///
/// Mirrors the slug the context pipeline uses when materializing a skill under
/// `.moa/skills/<slug>/`, so detection matches the exact path the model reads.
#[must_use]
pub fn skill_use_slug(skill_name: &str) -> String {
    let mut slug = String::new();
    let mut previous_was_separator = false;
    for character in skill_name.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            previous_was_separator = false;
        } else if !previous_was_separator && !slug.is_empty() {
            slug.push('-');
            previous_was_separator = true;
        }
    }
    slug.trim_matches('-').to_string()
}

fn collect_lowercased_strings(input: &Value, output: &mut Vec<String>) {
    match input {
        Value::String(value) => output.push(value.to_ascii_lowercase()),
        Value::Array(items) => {
            for item in items {
                collect_lowercased_strings(item, output);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_lowercased_strings(value, output);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn detects_skill_md_read_by_package_path() {
        // Pins: reading a selected skill's materialized SKILL.md counts as using it.
        let used = skills_used_in_tool_call(
            "file_read",
            &json!({"path": ".moa/skills/refund-policy/SKILL.md"}),
            &["Refund Policy".to_string(), "unrelated".to_string()],
        );
        assert_eq!(used, vec!["Refund Policy".to_string()]);
    }

    #[test]
    fn detects_package_resource_read_not_just_skill_md() {
        // Pins: any file under the skill's package directory counts, not only SKILL.md.
        let used = skills_used_in_tool_call(
            "bash",
            &json!({"cmd": "python .moa/skills/data-export/scripts/run.py"}),
            &["data-export".to_string()],
        );
        assert_eq!(used, vec!["data-export".to_string()]);
    }

    #[test]
    fn detects_skill_action_reference() {
        // Pins: a `skill://<name>` action reference in the input counts as use.
        let used = skills_used_in_tool_call(
            "invoke_action",
            &json!({"reference": "skill://greeter#welcome"}),
            &["greeter".to_string()],
        );
        assert_eq!(used, vec!["greeter".to_string()]);
    }

    #[test]
    fn injected_but_never_referenced_is_not_used() {
        // Pins: an injected skill the model never references is NOT recorded as used.
        let used = skills_used_in_tool_call(
            "bash",
            &json!({"cmd": "cargo test"}),
            &["refund-policy".to_string()],
        );
        assert!(used.is_empty());
    }

    #[test]
    fn only_selected_skills_are_returned() {
        // Pins: a package path for a non-selected skill is ignored (detection is scoped
        // to the turn's selected skills so a stray path cannot fabricate a use).
        let used = skills_used_in_tool_call(
            "file_read",
            &json!({"path": ".moa/skills/not-selected/SKILL.md"}),
            &["refund-policy".to_string()],
        );
        assert!(used.is_empty());
    }
}
