//! Safe execution mapping for trusted skill scripts kept outside mutable workspaces.

use std::path::{Component, Path};

use moa_core::{
    error::{MoaError, Result},
    types::{hands::validate_sandbox_file_path, tools::ToolOutput},
};
use serde_json::Value;

use crate::tools::docker_file::resolve_container_workspace_path;

/// One validated logical command and its provider-internal trusted-root path.
pub(crate) struct TrustedSkillCommand {
    logical: String,
    resolved: String,
}

impl TrustedSkillCommand {
    /// Returns the shell-quoted provider-internal command token.
    pub(crate) fn shell_token(&self) -> String {
        shell_words::quote(&self.resolved).into_owned()
    }

    /// Replaces provider-internal trusted-root paths in caller-visible output.
    pub(crate) fn redact_output(&self, output: &mut ToolOutput) {
        for content in &mut output.content {
            match content {
                moa_core::types::tools::ToolContent::Text { text } => {
                    *text = text.replace(&self.resolved, &self.logical);
                }
                moa_core::types::tools::ToolContent::Process { output } => {
                    output.stdout = output.stdout.replace(&self.resolved, &self.logical);
                    output.stderr = output.stderr.replace(&self.resolved, &self.logical);
                }
                moa_core::types::tools::ToolContent::Json { .. } => {}
            }
        }
    }
}

/// Resolves only one canonical trusted-skill executable token.
///
/// Ordinary shell programs return `None` unchanged. Any command that mentions
/// the trusted logical namespace must be exactly one path token: no arguments,
/// traversal, absolute path, quoting, expansion, or control operators.
pub(crate) fn resolve_trusted_skill_command(
    command: &str,
    trusted_root: &str,
) -> Result<Option<TrustedSkillCommand>> {
    if !command.contains(".moa/skills") {
        return Ok(None);
    }
    if !command.starts_with(".moa/skills/")
        || command
            .bytes()
            .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(invalid_trusted_command());
    }
    let components = Path::new(command).components().collect::<Vec<_>>();
    if components.len() < 4
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid_trusted_command());
    }
    validate_sandbox_file_path(command).map_err(|_| invalid_trusted_command())?;
    Ok(Some(TrustedSkillCommand {
        logical: command.to_string(),
        resolved: format!("{}/{command}", trusted_root.trim_end_matches('/')),
    }))
}

/// Normalizes a path that addresses the logical trusted-skill namespace.
///
/// Paths outside `.moa/skills` and paths containing traversal, roots, or
/// non-UTF-8 components return `None`.
pub(crate) fn normalized_trusted_skill_path(path: &str) -> Option<String> {
    let mut segments = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(segment) => segments.push(segment.to_str()?),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let normalized = segments.join("/");
    (normalized == ".moa/skills" || normalized.starts_with(".moa/skills/")).then_some(normalized)
}

/// Resolves a model-visible file path into either trusted authority or the
/// provider's mutable workspace root.
pub(crate) fn resolve_provider_file_path(
    path: &str,
    mutable_root: &str,
    trusted_root: &str,
) -> Result<String> {
    if let Some(normalized) = normalized_trusted_skill_path(path) {
        validate_sandbox_file_path(&normalized)?;
        return Ok(format!("{trusted_root}/{normalized}"));
    }
    resolve_container_workspace_path(mutable_root, path)
}

/// Rewrites only the validated `cmd` field while retaining the bounded timeout.
pub(crate) fn rewrite_bash_input(input: &str, command: &TrustedSkillCommand) -> Result<String> {
    let mut payload: Value = serde_json::from_str(input)?;
    let object = payload
        .as_object_mut()
        .ok_or_else(|| MoaError::ValidationError("bash input must be a JSON object".to_string()))?;
    object.insert("cmd".to_string(), Value::String(command.shell_token()));
    serde_json::to_string(&payload).map_err(Into::into)
}

fn invalid_trusted_command() -> MoaError {
    MoaError::ValidationError(
        "trusted skill execution requires one canonical `.moa/skills/<slug>/<file>` command token"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_skill_command_accepts_only_one_canonical_path_token_offline() {
        // Pins: trusted-root routing cannot be reached through traversal,
        // absolute paths, shell operators, expansion, quoting, or arguments.
        let command =
            resolve_trusted_skill_command(".moa/skills/review/scripts/run.sh", "/opt/moa/trusted")
                .expect("canonical skill command should validate")
                .expect("canonical skill command should map");
        assert_eq!(
            command.shell_token(),
            "/opt/moa/trusted/.moa/skills/review/scripts/run.sh"
        );
        assert!(
            resolve_trusted_skill_command("printf ok", "/opt/moa/trusted")
                .expect("ordinary bash remains outside trusted routing")
                .is_none()
        );

        for invalid in [
            "./.moa/skills/review/scripts/run.sh",
            ".moa/skills/../secrets/run.sh",
            "/.moa/skills/review/scripts/run.sh",
            ".moa/skills/review/scripts/run.sh --force",
            ".moa/skills/review/scripts/run.sh; id",
            ".moa/skills/review/scripts/run.sh|id",
            "$(.moa/skills/review/scripts/run.sh)",
            "echo .moa/skills/review/scripts/run.sh",
        ] {
            assert!(
                resolve_trusted_skill_command(invalid, "/opt/moa/trusted").is_err(),
                "noncanonical trusted command must fail closed: {invalid}"
            );
        }
    }

    #[test]
    fn provider_file_paths_stay_in_the_mutable_or_trusted_root_offline() {
        // Pins: cloud file tools use the same workspace-relative contract as
        // local and Docker tools without folding trusted skill authority into
        // checkpointed mutable bytes.
        assert_eq!(
            resolve_provider_file_path("notes.md", "/workspace", "/opt/moa/trusted")
                .expect("relative workspace path"),
            "/workspace/notes.md"
        );
        assert_eq!(
            resolve_provider_file_path(
                ".moa/skills/review/SKILL.md",
                "/workspace",
                "/opt/moa/trusted",
            )
            .expect("trusted skill path"),
            "/opt/moa/trusted/.moa/skills/review/SKILL.md"
        );
        assert!(
            resolve_provider_file_path("/etc/hosts", "/workspace", "/opt/moa/trusted").is_err()
        );
    }
}
