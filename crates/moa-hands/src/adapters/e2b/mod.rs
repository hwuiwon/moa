//! E2B-backed hand provider for microVM execution.

mod client;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use moa_config::MoaConfig;
use moa_core::{
    canonical_json::canonical_json_bytes, error::MoaError, error::Result, error::ToolFailureClass,
    traits::HandProvider, types::hands::DeadlineEnforcement, types::hands::EgressMode,
    types::hands::HandHandle, types::hands::HandProviderCapabilities, types::hands::HandSpec,
    types::hands::HandStatus, types::hands::ResourceSupport, types::hands::SandboxFile,
    types::hands::SandboxProfile, types::hands::SandboxTier, types::hands::SandboxTierCapabilities,
    types::hands::validate_sandbox_file_path, types::identifiers::HandProvisioningOperationId,
    types::tools::ToolOutput,
};
use reqwest::header::CONTENT_TYPE;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tokio::time::Instant;

use crate::adapters::http_util::{
    build_url, expect_success, expect_success_json, http_error, required_string_field,
};
use crate::tools::edit_output::{
    ExistingFileContent, build_file_write_output, build_text_edit_output,
};
use crate::tools::sandbox_descriptor::{
    SandboxToolCapability, supported_capability_for_tool, unsupported_tool,
};
use crate::tools::str_replace::plan_str_replace;
use crate::tools::{bash, file_outline, file_read, grep};

use client::{
    default_headers, encode_connect_request, envd_headers, parse_e2b_connect_stream, shell_escape,
};

const E2B_SUPPORTED_CAPABILITIES: &[SandboxToolCapability] = &SandboxToolCapability::ALL;
const DEFAULT_E2B_API_URL: &str = "https://api.e2b.dev";
const DEFAULT_E2B_DOMAIN: &str = "e2b.app";
const DEFAULT_E2B_TEMPLATE: &str = "base";
const DEFAULT_ENVD_PORT: u16 = 49983;
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_PROTOCOL_VERSION: &str = "1";
const E2B_PROVISIONING_OPERATION_METADATA_KEY: &str = "moa_provisioning_operation_id";
const E2B_PROVISIONING_SPEC_METADATA_KEY: &str = "moa_provisioning_spec_sha256";
const E2B_SANDBOX_LIST_LIMIT: usize = 100;

#[derive(Debug, Clone)]
pub(super) struct ConnectedSandbox {
    pub(super) sandbox_domain: String,
    pub(super) envd_access_token: String,
    pub(super) _envd_version: String,
}

/// E2B cloud hand provider for microVM-backed execution.
pub struct E2BHandProvider {
    client: reqwest::Client,
    api_url: String,
    sandbox_domain: String,
    default_template: String,
    sandbox_base_url_override: Option<String>,
    sandboxes: RwLock<HashMap<String, ConnectedSandbox>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProvisionedE2BSandbox {
    sandbox_id: String,
    spec_fingerprint: Option<String>,
}

impl E2BHandProvider {
    /// Creates a new E2B provider from an API key.
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Self::with_api_url(
            api_key,
            DEFAULT_E2B_API_URL,
            DEFAULT_E2B_DOMAIN,
            DEFAULT_E2B_TEMPLATE,
        )
    }

    /// Creates an E2B provider from the loaded MOA config.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        let hands = config
            .cloud
            .hands
            .as_ref()
            .ok_or_else(|| MoaError::ConfigError("missing [cloud.hands] config".to_string()))?;
        let api_key = hands
            .e2b_api_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                MoaError::MissingEnvironmentVariable("MOA_CLOUD_HANDS_E2B_API_KEY".to_string())
            })?
            .to_string();
        Self::with_api_url(
            api_key,
            hands.e2b_api_url.as_deref().unwrap_or(DEFAULT_E2B_API_URL),
            hands.e2b_domain.as_deref().unwrap_or(DEFAULT_E2B_DOMAIN),
            hands
                .e2b_template
                .as_deref()
                .unwrap_or(DEFAULT_E2B_TEMPLATE),
        )
    }

    /// Creates a provider with explicit API URL, domain, and template overrides.
    pub fn with_api_url(
        api_key: impl Into<String>,
        api_url: impl Into<String>,
        sandbox_domain: impl Into<String>,
        default_template: impl Into<String>,
    ) -> Result<Self> {
        let api_key = api_key.into();
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_COMMAND_TIMEOUT)
            .default_headers(default_headers(&api_key)?)
            .build()
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to build E2B client: {error}"))
            })?;
        Ok(Self {
            client,
            api_url: api_url.into().trim_end_matches('/').to_string(),
            sandbox_domain: sandbox_domain.into(),
            default_template: default_template.into(),
            sandbox_base_url_override: None,
            sandboxes: RwLock::new(HashMap::new()),
        })
    }

    /// Overrides the computed envd sandbox base URL. Intended for tests and local proxies.
    #[cfg(test)]
    pub fn with_sandbox_base_url(mut self, sandbox_base_url: impl Into<String>) -> Self {
        self.sandbox_base_url_override =
            Some(sandbox_base_url.into().trim_end_matches('/').to_string());
        self
    }

    async fn create_sandbox(
        &self,
        spec: &HandSpec,
        translated: E2BProfileFields,
        spec_fingerprint: &str,
    ) -> Result<String> {
        let metadata = HashMap::from([
            (
                E2B_PROVISIONING_OPERATION_METADATA_KEY,
                spec.provisioning_operation_id.to_string(),
            ),
            (
                E2B_PROVISIONING_SPEC_METADATA_KEY,
                spec_fingerprint.to_string(),
            ),
        ]);
        let response = self
            .client
            .post(format!("{}/sandboxes", self.api_url))
            .json(&json!({
                "templateID": spec.image.clone().unwrap_or_else(|| self.default_template.clone()),
                "envVars": spec.env,
                "timeout": translated.timeout_secs,
                "secure": true,
                "allow_internet_access": translated.allow_internet_access,
                "autoPause": true,
                "autoResume": { "enabled": true },
                "metadata": metadata,
            }))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to create E2B sandbox: {error}"))
            })?;
        let value = expect_success_json(response, "E2B").await?;
        let sandbox_id = value
            .get("sandboxID")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                MoaError::ProviderError("E2B create sandbox response missing sandboxID".to_string())
            })?
            .to_string();
        self.sandboxes.write().await.insert(
            sandbox_id.clone(),
            ConnectedSandbox {
                sandbox_domain: value
                    .get("domain")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.sandbox_domain)
                    .to_string(),
                envd_access_token: required_string_field(&value, "envdAccessToken")?.to_string(),
                _envd_version: required_string_field(&value, "envdVersion")?.to_string(),
            },
        );
        Ok(sandbox_id)
    }

    fn provisioning_spec_fingerprint(
        &self,
        spec: &HandSpec,
        translated: E2BProfileFields,
    ) -> Result<String> {
        let creation_contract = json!({
            "templateID": spec.image.as_deref().unwrap_or(&self.default_template),
            "envVars": spec.env,
            "timeout": translated.timeout_secs,
            "secure": true,
            "allow_internet_access": translated.allow_internet_access,
            "autoPause": true,
            "autoResume": { "enabled": true },
        });
        let canonical = canonical_json_bytes(&creation_contract).map_err(|error| {
            MoaError::ProviderError(format!(
                "failed to canonicalize E2B provisioning spec: {error}"
            ))
        })?;
        Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
    }

    async fn provisioned_sandboxes(
        &self,
        operation_id: HandProvisioningOperationId,
    ) -> Result<Vec<ProvisionedE2BSandbox>> {
        let metadata_query = format!("{E2B_PROVISIONING_OPERATION_METADATA_KEY}={operation_id}");
        let page_limit = E2B_SANDBOX_LIST_LIMIT.to_string();
        let mut next_token: Option<String> = None;
        let mut seen_tokens = HashSet::new();
        let mut seen_sandbox_ids = HashSet::new();
        let mut sandboxes = Vec::new();

        loop {
            let mut url = build_url(
                &format!("{}/v2/sandboxes", self.api_url),
                &[
                    ("metadata", metadata_query.as_str()),
                    ("limit", page_limit.as_str()),
                    ("state", "running,paused"),
                ],
                "E2B",
            )?;
            if let Some(token) = next_token.as_deref() {
                url.query_pairs_mut().append_pair("nextToken", token);
            }
            let response = self.client.get(url).send().await.map_err(|error| {
                MoaError::ProviderError(format!(
                    "failed to list E2B provisioning operation sandboxes: {error}"
                ))
            })?;
            if !response.status().is_success() {
                return Err(http_error(response).await);
            }
            let response_next_token = response
                .headers()
                .get("x-next-token")
                .map(|value| {
                    value
                        .to_str()
                        .map(str::trim)
                        .map(str::to_string)
                        .map_err(|error| {
                            MoaError::ProviderError(format!(
                                "invalid E2B X-Next-Token response header: {error}"
                            ))
                        })
                })
                .transpose()?
                .filter(|token| !token.is_empty());
            let value = expect_success_json(response, "E2B").await?;
            let page = value.as_array().ok_or_else(|| {
                MoaError::ProviderError(
                    "E2B list sandboxes response must be a JSON array".to_string(),
                )
            })?;

            for item in page {
                let sandbox = decode_provisioned_sandbox(item, operation_id)?;
                if seen_sandbox_ids.insert(sandbox.sandbox_id.clone()) {
                    sandboxes.push(sandbox);
                }
            }

            match response_next_token {
                Some(token) => {
                    if !seen_tokens.insert(token.clone()) {
                        return Err(MoaError::ProviderError(
                            "E2B sandbox pagination repeated a continuation token".to_string(),
                        ));
                    }
                    next_token = Some(token);
                }
                None if page.len() == E2B_SANDBOX_LIST_LIMIT => {
                    return Err(MoaError::ProviderError(format!(
                        "E2B returned a full {E2B_SANDBOX_LIST_LIMIT}-sandbox page without an X-Next-Token; refusing to truncate provisioning recovery"
                    )));
                }
                None => break,
            }
        }

        sandboxes.sort_unstable_by(|left, right| left.sandbox_id.cmp(&right.sandbox_id));
        Ok(sandboxes)
    }

    async fn connect_sandbox(&self, sandbox_id: &str) -> Result<ConnectedSandbox> {
        let response = self
            .client
            .post(format!("{}/sandboxes/{sandbox_id}/connect", self.api_url))
            .json(&json!({
                "timeout": DEFAULT_COMMAND_TIMEOUT.as_secs(),
            }))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to connect E2B sandbox: {error}"))
            })?;
        let value = expect_success_json(response, "E2B").await?;
        let sandbox = ConnectedSandbox {
            sandbox_domain: value
                .get("domain")
                .and_then(Value::as_str)
                .unwrap_or(&self.sandbox_domain)
                .to_string(),
            envd_access_token: required_string_field(&value, "envdAccessToken")?.to_string(),
            _envd_version: required_string_field(&value, "envdVersion")?.to_string(),
        };
        self.sandboxes
            .write()
            .await
            .insert(sandbox_id.to_string(), sandbox.clone());
        Ok(sandbox)
    }

    async fn connected_sandbox(&self, sandbox_id: &str) -> Result<ConnectedSandbox> {
        if let Some(sandbox) = self.sandboxes.read().await.get(sandbox_id).cloned() {
            return Ok(sandbox);
        }
        self.connect_sandbox(sandbox_id).await
    }

    fn envd_url(&self, sandbox_id: &str, sandbox: &ConnectedSandbox) -> String {
        if let Some(base_url) = &self.sandbox_base_url_override {
            return base_url.clone();
        }
        format!(
            "https://{}-{}.{}",
            DEFAULT_ENVD_PORT, sandbox_id, sandbox.sandbox_domain
        )
    }

    async fn execute_bash(
        &self,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        cmd: &str,
        timeout: Duration,
    ) -> Result<ToolOutput> {
        let started_at = Instant::now();
        let url = format!(
            "{}/process.Process/Start",
            self.envd_url(sandbox_id, sandbox)
        );
        tokio::time::timeout(timeout, async {
            let response = self
                .client
                .post(url)
                .headers(envd_headers(sandbox_id, sandbox)?)
                .header(CONTENT_TYPE, "application/connect+json")
                .header("Connect-Protocol-Version", CONNECT_PROTOCOL_VERSION)
                .body(encode_connect_request(&json!({
                    "process": {
                        "cmd": "/bin/bash",
                        "args": ["-l", "-c", cmd],
                        "envs": {},
                    },
                    "stdin": false,
                }))?)
                .send()
                .await
                .map_err(|error| {
                    MoaError::ProviderError(format!("failed to start E2B command: {error}"))
                })?;
            if !response.status().is_success() {
                return Err(http_error(response).await);
            }
            let body = response.bytes().await.map_err(|error| {
                MoaError::ProviderError(format!("failed to read E2B command body: {error}"))
            })?;
            parse_e2b_connect_stream(&body, started_at.elapsed())
        })
        .await
        .map_err(|_| {
            MoaError::ToolError(format!(
                "E2B command timed out after {}s",
                timeout.as_secs()
            ))
        })?
    }

    async fn read_file(
        &self,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
    ) -> Result<ToolOutput> {
        let started_at = Instant::now();
        let url = build_url(
            &format!("{}/files", self.envd_url(sandbox_id, sandbox)),
            &[("path", path)],
            "E2B",
        )?;
        let response = self
            .client
            .get(url)
            .headers(envd_headers(sandbox_id, sandbox)?)
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to read E2B file: {error}"))
            })?;
        if !response.status().is_success() {
            return Err(http_error(response).await);
        }
        Ok(ToolOutput::text(
            response.text().await.map_err(|error| {
                MoaError::ProviderError(format!("failed to decode E2B file response: {error}"))
            })?,
            started_at.elapsed(),
        ))
    }

    async fn write_file(
        &self,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
        content: &str,
    ) -> Result<ToolOutput> {
        let existing = match self.read_file(sandbox_id, sandbox, path).await {
            Ok(output) => ExistingFileContent::Text(output.to_text()),
            Err(MoaError::HttpStatus { status: 404, .. }) => ExistingFileContent::Missing,
            Err(error) => return Err(error),
        };
        let duration = self
            .upload_file(sandbox_id, sandbox, path, content.as_bytes())
            .await?;
        Ok(build_file_write_output(path, &existing, content, duration))
    }

    async fn upload_file(
        &self,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
        content: &[u8],
    ) -> Result<Duration> {
        let started_at = Instant::now();
        let url = build_url(
            &format!("{}/files", self.envd_url(sandbox_id, sandbox)),
            &[("path", path)],
            "E2B",
        )?;
        let response = self
            .client
            .post(url)
            .headers(envd_headers(sandbox_id, sandbox)?)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(content.to_vec())
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to write E2B file: {error}"))
            })?;
        let _ = expect_success_json(response, "E2B").await?;
        Ok(started_at.elapsed())
    }

    async fn chmod_file(
        &self,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
    ) -> Result<()> {
        let output = self
            .execute_bash(
                sandbox_id,
                sandbox,
                &format!("chmod 755 {}", shell_escape(path)),
                DEFAULT_COMMAND_TIMEOUT,
            )
            .await?;
        if output.is_error {
            return Err(MoaError::ProviderError(format!(
                "failed to mark E2B file executable `{path}`: {}",
                output.to_text()
            )));
        }
        Ok(())
    }

    async fn str_replace_file(
        &self,
        sandbox_id: &str,
        sandbox: &ConnectedSandbox,
        path: &str,
        input: &str,
    ) -> Result<ToolOutput> {
        let existing_content = match self.read_file(sandbox_id, sandbox, path).await {
            Ok(output) => Some(output.to_text()),
            Err(MoaError::HttpStatus { status: 404, .. }) => None,
            Err(error) => return Err(error),
        };
        let planned = plan_str_replace(input, existing_content.as_deref(), path, 4)?;
        let duration = self
            .upload_file(
                sandbox_id,
                sandbox,
                path,
                planned.updated_content.as_bytes(),
            )
            .await?;
        Ok(build_text_edit_output(
            path,
            existing_content.as_deref().unwrap_or_default(),
            &planned.updated_content,
            duration,
        ))
    }
}

/// Extracts the sandbox id from an E2B hand handle.
fn sandbox_id(handle: &HandHandle) -> Result<&str> {
    match handle {
        HandHandle::E2B { sandbox_id } => Ok(sandbox_id.as_str()),
        _ => Err(MoaError::Unsupported(
            "non-E2B hand handle passed to E2BHandProvider".to_string(),
        )),
    }
}

fn decode_provisioned_sandbox(
    value: &Value,
    operation_id: HandProvisioningOperationId,
) -> Result<ProvisionedE2BSandbox> {
    let expected_operation_id = operation_id.to_string();
    let sandbox_id = required_string_field(value, "sandboxID")?.to_string();
    let metadata = value
        .get("metadata")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            MoaError::ProviderError(format!(
                "E2B sandbox `{sandbox_id}` matched operation `{operation_id}` without metadata"
            ))
        })?;
    let actual_operation_id = metadata
        .get(E2B_PROVISIONING_OPERATION_METADATA_KEY)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            MoaError::ProviderError(format!(
                "E2B sandbox `{sandbox_id}` omitted `{E2B_PROVISIONING_OPERATION_METADATA_KEY}`"
            ))
        })?;
    if actual_operation_id != expected_operation_id.as_str() {
        return Err(MoaError::ProviderError(format!(
            "E2B sandbox `{sandbox_id}` returned for operation `{operation_id}` carries operation `{actual_operation_id}`"
        )));
    }
    let spec_fingerprint = metadata
        .get(E2B_PROVISIONING_SPEC_METADATA_KEY)
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(ProvisionedE2BSandbox {
        sandbox_id,
        spec_fingerprint,
    })
}

/// Revision of the E2B provider's capability declaration.
pub const E2B_CAPABILITIES_REVISION: &str = "e2b-hands-v1";

/// What E2B can enforce for a microVM sandbox.
///
/// Only two of the six dimensions map onto documented sandbox-create fields:
/// `timeout`, which E2B enforces as the sandbox's maximum lifetime, and
/// `allow_internet_access`, which is a whole-sandbox on/off switch. CPU,
/// memory, and disk are fixed by the template at build time and are not
/// settable per sandbox, so a bounded request for any of them is refused rather
/// than serialized into a field E2B would ignore. There is no per-destination
/// filter, so an egress allowlist is refused too, and no idle field, so the
/// durable reaper owns the idle deadline.
pub static E2B_CAPABILITIES: LazyLock<HandProviderCapabilities> =
    LazyLock::new(|| HandProviderCapabilities {
        revision: E2B_CAPABILITIES_REVISION.to_string(),
        tiers: vec![SandboxTierCapabilities {
            tier: SandboxTier::MicroVM,
            cpu: ResourceSupport::unbounded_only(),
            memory: ResourceSupport::unbounded_only(),
            ephemeral_disk: ResourceSupport::unbounded_only(),
            egress_modes: vec![EgressMode::DenyAll, EgressMode::Unrestricted],
            idle_enforcement: DeadlineEnforcement::DurableReaper,
            max_lifetime_enforcement: DeadlineEnforcement::Provider,
        }],
    });

/// E2B's smallest accepted sandbox timeout, in seconds.
const E2B_MIN_TIMEOUT_SECS: u64 = 60;

/// The sandbox-create fields E2B actually honors, translated from a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct E2BProfileFields {
    timeout_secs: u64,
    allow_internet_access: bool,
}

impl E2BProfileFields {
    /// Translates the enforceable dimensions and refuses the rest.
    fn translate(profile: &SandboxProfile) -> Result<Self> {
        reject_unsupported_e2b_resource("CPU", profile.cpu.bounded_millicores().is_some())?;
        reject_unsupported_e2b_resource("memory", profile.memory.bounded_mebibytes().is_some())?;
        reject_unsupported_e2b_resource(
            "ephemeral disk",
            profile.ephemeral_disk.bounded_mebibytes().is_some(),
        )?;
        let allow_internet_access = match profile.egress.mode() {
            EgressMode::DenyAll => false,
            EgressMode::Unrestricted => true,
            EgressMode::AllowList => {
                return Err(MoaError::Unsupported(
                    "E2B sandboxes cannot enforce a per-destination egress allowlist".to_string(),
                ));
            }
        };
        // An unbounded maximum lifetime still has to become a number here,
        // because E2B has no "no timeout" value. Refusing is the honest answer:
        // a sandbox MOA believes is unbounded must not silently acquire E2B's
        // own deadline.
        let seconds = profile.max_lifetime.bounded_seconds().ok_or_else(|| {
            MoaError::Unsupported(
                "E2B sandboxes always carry a maximum lifetime and cannot serve an unbounded one"
                    .to_string(),
            )
        })?;
        if seconds.get() < E2B_MIN_TIMEOUT_SECS {
            return Err(MoaError::Unsupported(format!(
                "E2B sandboxes require a maximum lifetime of at least {E2B_MIN_TIMEOUT_SECS}s, \
                 requested {seconds}s"
            )));
        }
        Ok(Self {
            timeout_secs: seconds.get(),
            allow_internet_access,
        })
    }
}

/// Refuses a bounded resource dimension E2B fixes at template build time.
fn reject_unsupported_e2b_resource(dimension: &str, bounded: bool) -> Result<()> {
    if bounded {
        return Err(MoaError::Unsupported(format!(
            "E2B fixes {dimension} in the sandbox template and cannot honor a per-sandbox bound"
        )));
    }
    Ok(())
}

#[async_trait]
impl HandProvider for E2BHandProvider {
    fn provider_name(&self) -> &str {
        "e2b"
    }

    fn capabilities(&self) -> HandProviderCapabilities {
        E2B_CAPABILITIES.clone()
    }

    async fn provision(&self, spec: HandSpec) -> Result<HandHandle> {
        if !matches!(spec.sandbox_tier, SandboxTier::MicroVM) {
            return Err(MoaError::Unsupported(
                "E2B provider is reserved for microvm sandboxes".to_string(),
            ));
        }
        let translated = E2BProfileFields::translate(spec.effective_profile.profile())?;
        let expected_fingerprint = self.provisioning_spec_fingerprint(&spec, translated)?;
        let existing = self
            .provisioned_sandboxes(spec.provisioning_operation_id)
            .await?;
        if existing.len() > 1 {
            return Err(MoaError::ProviderError(format!(
                "E2B provisioning operation `{}` resolved {} sandboxes; durable reaper cleanup is required",
                spec.provisioning_operation_id,
                existing.len()
            )));
        }
        if let Some(existing) = existing.first() {
            if existing.spec_fingerprint.as_deref() != Some(expected_fingerprint.as_str()) {
                return Err(MoaError::ProviderError(format!(
                    "E2B provisioning operation `{}` was reused with a different creation spec",
                    spec.provisioning_operation_id
                )));
            }
            return Ok(HandHandle::e2b(existing.sandbox_id.clone()));
        }
        let sandbox_id = self
            .create_sandbox(&spec, translated, &expected_fingerprint)
            .await?;
        Ok(HandHandle::e2b(sandbox_id))
    }

    async fn provisioned_hands(
        &self,
        operation_id: HandProvisioningOperationId,
    ) -> Result<Vec<HandHandle>> {
        Ok(self
            .provisioned_sandboxes(operation_id)
            .await?
            .into_iter()
            .map(|sandbox| HandHandle::e2b(sandbox.sandbox_id))
            .collect())
    }

    async fn execute(&self, handle: &HandHandle, tool: &str, input: &str) -> Result<ToolOutput> {
        let sandbox_id = sandbox_id(handle)?;
        let sandbox = self.connected_sandbox(sandbox_id).await?;
        let payload: Value = serde_json::from_str(input)?;
        match supported_capability_for_tool(tool, E2B_SUPPORTED_CAPABILITIES) {
            Some(SandboxToolCapability::Bash) => {
                let params = bash::BashToolInput::parse(input)?;
                // This adapter implements only the unbounded `execute`, so no run
                // deadline reaches it: both the sandbox lifetime and the run
                // budget are absent here. Opting E2B into `execute_bounded` is
                // what would carry them.
                let timeout = params.timeout(DEFAULT_COMMAND_TIMEOUT, None, None);
                self.execute_bash(sandbox_id, &sandbox, &params.cmd, timeout)
                    .await
            }
            Some(SandboxToolCapability::Grep) => {
                let command = grep::remote_shell_command(input, "/")?;
                self.execute_bash(sandbox_id, &sandbox, &command, DEFAULT_COMMAND_TIMEOUT)
                    .await
            }
            Some(SandboxToolCapability::FileOutline) => {
                let path = required_string_field(&payload, "path")?;
                let content = self.read_file(sandbox_id, &sandbox, path).await?.to_text();
                file_outline::execute_with_content(input, path, &content)
            }
            Some(SandboxToolCapability::FileRead) => {
                let path = required_string_field(&payload, "path")?;
                let content = self.read_file(sandbox_id, &sandbox, path).await?.to_text();
                file_read::execute_with_content(input, path, &content)
            }
            Some(SandboxToolCapability::StrReplace) => {
                self.str_replace_file(
                    sandbox_id,
                    &sandbox,
                    required_string_field(&payload, "path")?,
                    input,
                )
                .await
            }
            Some(SandboxToolCapability::FileWrite) => {
                self.write_file(
                    sandbox_id,
                    &sandbox,
                    required_string_field(&payload, "path")?,
                    required_string_field(&payload, "content")?,
                )
                .await
            }
            Some(SandboxToolCapability::FileSearch) => {
                let pattern = shell_escape(required_string_field(&payload, "pattern")?);
                self.execute_bash(
                    sandbox_id,
                    &sandbox,
                    &format!("find / -name {pattern} -print 2>/dev/null || true"),
                    DEFAULT_COMMAND_TIMEOUT,
                )
                .await
            }
            None => Err(unsupported_tool("E2B", tool)),
        }
    }

    async fn install_files(&self, handle: &HandHandle, files: &[SandboxFile]) -> Result<()> {
        let sandbox_id = sandbox_id(handle)?;
        let sandbox = self.connected_sandbox(sandbox_id).await?;
        for file in files {
            validate_sandbox_file_path(&file.path)?;
            self.upload_file(sandbox_id, &sandbox, &file.path, &file.content)
                .await?;
            if file.executable {
                self.chmod_file(sandbox_id, &sandbox, &file.path).await?;
            }
        }
        Ok(())
    }

    async fn classify_error(
        &self,
        handle: &HandHandle,
        error: &MoaError,
        consecutive_timeouts: u32,
    ) -> ToolFailureClass {
        let status = self.status(handle).await.ok();
        client::classify_error(error, status, consecutive_timeouts)
    }

    async fn health_check(&self, handle: &HandHandle) -> Result<bool> {
        Ok(matches!(
            self.status(handle).await?,
            HandStatus::Running | HandStatus::Paused | HandStatus::Provisioning
        ))
    }

    async fn status(&self, handle: &HandHandle) -> Result<HandStatus> {
        let sandbox_id = sandbox_id(handle)?;
        let response = self
            .client
            .get(format!("{}/sandboxes/{sandbox_id}", self.api_url))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to inspect E2B sandbox: {error}"))
            })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(HandStatus::Destroyed);
        }
        let value = expect_success_json(response, "E2B").await?;
        let state = value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("running")
            .to_ascii_lowercase();
        Ok(match state.as_str() {
            "running" | "started" => HandStatus::Running,
            "paused" | "stopped" => HandStatus::Stopped,
            "provisioning" | "starting" => HandStatus::Provisioning,
            "ended" | "deleted" => HandStatus::Destroyed,
            "error" => HandStatus::Failed,
            _ => HandStatus::Running,
        })
    }

    async fn pause(&self, handle: &HandHandle) -> Result<()> {
        let sandbox_id = sandbox_id(handle)?;
        let response = self
            .client
            .post(format!("{}/sandboxes/{sandbox_id}/pause", self.api_url))
            .json(&json!({}))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to pause E2B sandbox: {error}"))
            })?;
        expect_success(response).await?;
        Ok(())
    }

    async fn resume(&self, handle: &HandHandle) -> Result<()> {
        let sandbox_id = sandbox_id(handle)?;
        let _ = self.connect_sandbox(sandbox_id).await?;
        Ok(())
    }

    async fn destroy(&self, handle: &HandHandle) -> Result<()> {
        let sandbox_id = sandbox_id(handle)?;
        let response = self
            .client
            .delete(format!("{}/sandboxes/{sandbox_id}", self.api_url))
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to destroy E2B sandbox: {error}"))
            })?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            self.sandboxes.write().await.remove(sandbox_id);
            return Ok(());
        }
        Err(http_error(response).await)
    }
}

#[cfg(test)]
mod operation_intent_tests {
    use moa_core::types::identifiers::HandProvisioningOperationId;
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        E2B_PROVISIONING_OPERATION_METADATA_KEY, E2B_PROVISIONING_SPEC_METADATA_KEY,
        decode_provisioned_sandbox,
    };

    #[test]
    fn decodes_operation_identity_and_spec_fingerprint() {
        // Pins: recovery accepts only resources carrying the exact durable
        // operation identity returned by E2B's metadata-filtered list API.
        let operation_id = HandProvisioningOperationId(Uuid::from_u128(0xe2b));
        let value = json!({
            "sandboxID": "sandbox-2",
            "metadata": {
                (E2B_PROVISIONING_OPERATION_METADATA_KEY): operation_id.to_string(),
                (E2B_PROVISIONING_SPEC_METADATA_KEY): "sha256:spec",
            },
        });

        let decoded = decode_provisioned_sandbox(&value, operation_id)
            .expect("matching operation metadata should decode");

        assert_eq!(decoded.sandbox_id, "sandbox-2");
        assert_eq!(decoded.spec_fingerprint.as_deref(), Some("sha256:spec"));
    }

    #[test]
    fn rejects_a_list_result_with_a_different_operation_identity() {
        // Pins: a provider-side filtering error cannot make the reaper destroy
        // a sandbox belonging to a different durable operation.
        let operation_id = HandProvisioningOperationId(Uuid::from_u128(0xe2b));
        let other_operation_id = HandProvisioningOperationId(Uuid::from_u128(0xbad));
        let value = json!({
            "sandboxID": "sandbox-other",
            "metadata": {
                (E2B_PROVISIONING_OPERATION_METADATA_KEY): other_operation_id.to_string(),
            },
        });

        let error = decode_provisioned_sandbox(&value, operation_id)
            .expect_err("mismatched operation metadata must fail closed");

        assert!(error.to_string().contains(&other_operation_id.to_string()));
    }
}
