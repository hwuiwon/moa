//! Constrained HTTP execution for tenant-installed connector actions.
//!
//! Every attempt is re-authorized and pinned to an immutable connection
//! generation, compiled binding, admitted destination, and durable replay row.
//! Model input can populate only reviewed path segments, query parameters, and
//! a JSON body; it can never select an origin, method, header, or credential.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt as _;
use moa_artifacts::connector::{
    HttpMethodV1, HttpOperationContract, RuntimeConnectorAuthRequirementV1,
};
use moa_core::traits::CredentialVault;
use moa_core::types::credentials::{
    CredentialContext, CredentialIdentity, CredentialKind, CredentialOperation,
    CredentialPrincipal, CredentialSlotName, RedactedSecret,
};
use moa_security::outbound_http::{
    AdmittedHttpDestination, OutboundHttpClientLimits, OutboundHttpPolicy,
    build_admitted_http_client,
};
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;
use zeroize::Zeroizing;

use crate::domain::{
    ConnectionOrigin, ConnectorConnection, InstalledActionBinding, OperationContractHash,
};
use crate::executor::{
    AuthorizedConnectorInvocation, ConnectorActionInvocation, ConnectorActionRuntime,
    ConnectorInvocationCoordinator, RawConnectorActionResult, ReservedConnectorInvocation,
};
use crate::repository::{ConnectionLifecycleRepository, ConnectorInvocationRepository};
use crate::{Error, Result};

const MAX_COMPLETE_URL_BYTES: usize = 8 * 1024;
const MAX_CREDENTIAL_HEADER_BYTES: usize = 8 * 1024;
const MAX_RESPONSE_HEADER_COUNT: usize = 128;
const MAX_RESPONSE_HEADER_BYTES: usize = 64 * 1024;

/// Production constrained-HTTP runtime for installed connector actions.
#[derive(Clone)]
pub struct HttpConnectorRuntime {
    coordinator: ConnectorInvocationCoordinator,
    credential_vault: Arc<dyn CredentialVault>,
    destination_policy: OutboundHttpPolicy,
}

impl HttpConnectorRuntime {
    /// Composes lifecycle state, the durable replay ledger, the credential vault,
    /// and outbound destination policy used by every HTTP attempt.
    #[must_use]
    pub fn new(
        lifecycle: Arc<dyn ConnectionLifecycleRepository>,
        invocations: Arc<dyn ConnectorInvocationRepository>,
        credential_vault: Arc<dyn CredentialVault>,
        destination_policy: OutboundHttpPolicy,
    ) -> Self {
        Self::with_coordinator(
            ConnectorInvocationCoordinator::new(lifecycle, invocations),
            credential_vault,
            destination_policy,
        )
    }

    /// Composes an already-shared invocation coordinator with HTTP transport dependencies.
    #[must_use]
    pub fn with_coordinator(
        coordinator: ConnectorInvocationCoordinator,
        credential_vault: Arc<dyn CredentialVault>,
        destination_policy: OutboundHttpPolicy,
    ) -> Self {
        Self {
            coordinator,
            credential_vault,
            destination_policy,
        }
    }

    /// Returns the shared authorization and invocation-ledger coordinator.
    #[must_use]
    pub const fn coordinator(&self) -> &ConnectorInvocationCoordinator {
        &self.coordinator
    }

    async fn invoke_inner(
        &self,
        invocation: ConnectorActionInvocation,
        prepared: crate::executor::PreparedConnectorAction,
    ) -> Result<RawConnectorActionResult> {
        let authorized = self.coordinator.authorize(invocation, prepared).await?;
        self.invoke_authorized(authorized).await
    }

    /// Executes one invocation already authorized and pinned by the shared coordinator.
    ///
    /// A composite dispatcher uses this entrypoint after inspecting the opaque
    /// carrier's typed runtime, avoiding a second catalog authorization pass.
    pub async fn invoke_authorized(
        &self,
        authorized: AuthorizedConnectorInvocation,
    ) -> Result<RawConnectorActionResult> {
        let started = Instant::now();
        let contract = authorized.operation().clone();
        let deadline = started + Duration::from_millis(u64::from(contract.total_timeout_ms));
        let cancellation = authorized.invocation().cancellation_token.clone();

        let origin = connection_origin(authorized.connection())?;
        let connect_timeout = bounded_connect_timeout(&contract, deadline)?;
        let destination =
            await_before_send(&cancellation, deadline, "destination_admission", async {
                self.destination_policy
                    .admit(origin.as_str(), connect_timeout)
                    .await
                    .map_err(|_| Error::Http {
                        code: "destination_rejected",
                    })
            })
            .await?;

        let request_url =
            build_request_url(&destination, &contract, &authorized.invocation().input)?;
        let request_body = build_request_body(&contract, &authorized.invocation().input)?;
        let auth = select_auth_requirement(authorized.binding(), &contract)?;
        let client = build_client(&destination, &contract, deadline)?;

        let request_hash = ConnectorInvocationCoordinator::request_hash(&authorized)?;
        let upstream_idempotency_key = contract
            .upstream_idempotency_header
            .as_ref()
            .map(|_| authorized.invocation().tool_call_id.to_string());
        let reserved = await_before_send(
            &cancellation,
            deadline,
            "invocation_reservation",
            self.coordinator
                .reserve(authorized, request_hash, upstream_idempotency_key.clone()),
        )
        .await?;

        self.prepare_and_send(
            reserved,
            &contract,
            client,
            request_url,
            request_body,
            auth,
            upstream_idempotency_key,
            request_hash,
            deadline,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_and_send(
        &self,
        reserved: ReservedConnectorInvocation,
        contract: &HttpOperationContract,
        client: reqwest::Client,
        request_url: Url,
        request_body: Option<Vec<u8>>,
        auth: SelectedAuth,
        upstream_idempotency_key: Option<String>,
        request_hash: crate::domain::OperationContractHash,
        deadline: Instant,
    ) -> Result<RawConnectorActionResult> {
        let invocation = reserved.authorized().invocation();
        let cancellation = invocation.cancellation_token.clone();
        if cancellation.is_cancelled() {
            return reserved
                .fail_before_send(
                    Error::Cancelled {
                        stage: "before_credential_resolution",
                    },
                    "cancelled_before_send",
                )
                .await;
        }

        let credential_header = match self
            .resolve_credential_header(invocation, auth, request_hash, deadline)
            .await
        {
            Ok(header) => header,
            Err(error) => {
                return reserved
                    .fail_before_send(error, "credential_resolution_failed")
                    .await;
            }
        };

        let rechecked = await_before_send(
            &cancellation,
            deadline,
            "state_recheck",
            self.coordinator.recheck(&reserved),
        )
        .await;
        if let Err(error) = rechecked {
            return reserved
                .fail_before_send(error, "state_recheck_failed")
                .await;
        }

        let request = match build_request(
            &client,
            contract,
            request_url,
            request_body,
            credential_header,
            upstream_idempotency_key,
        ) {
            Ok(request) => request,
            Err(error) => {
                return reserved
                    .fail_before_send(error, "request_construction_failed")
                    .await;
            }
        };

        if cancellation.is_cancelled() {
            return reserved
                .fail_before_send(
                    Error::Cancelled {
                        stage: "before_transmission",
                    },
                    "cancelled_before_send",
                )
                .await;
        }
        if Instant::now() >= deadline {
            return reserved
                .fail_before_send(
                    Error::Http {
                        code: "total_timeout_before_send",
                    },
                    "total_timeout_before_send",
                )
                .await;
        }

        // The type-state transition is the final operation before send.
        let transmitting = reserved.mark_transmitting().await?;

        let response = tokio::select! {
            _ = cancellation.cancelled() => {
                return transmitting.unknown(
                    Error::Cancelled { stage: "transmission" },
                    "cancelled_during_transmission",
                ).await;
            }
            response = request.send() => response.map_err(|error| Error::Http {
                code: if error.is_timeout() {
                    "total_timeout_during_transmission"
                } else {
                    "transport_failed"
                },
            }),
            _ = tokio::time::sleep_until(deadline) => Err(Error::Http {
                code: "total_timeout_during_transmission",
            }),
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                return transmitting
                    .unknown(error, "transport_outcome_unknown")
                    .await;
            }
        };

        let output = match read_response(response, contract, &cancellation, deadline).await {
            Ok(output) => output,
            Err(error) => {
                return transmitting.fail(error, "upstream_response_rejected").await;
            }
        };
        if let Err(error) = transmitting.validate_output(&output) {
            return transmitting.fail(error, "upstream_response_rejected").await;
        }

        Ok(transmitting.succeed_raw(output))
    }

    async fn resolve_credential_header(
        &self,
        invocation: &ConnectorActionInvocation,
        auth: SelectedAuth,
        request_hash: OperationContractHash,
        deadline: Instant,
    ) -> Result<Option<(HeaderName, HeaderValue)>> {
        let SelectedAuth::Credential {
            slot,
            kind,
            header,
            bearer,
        } = auth
        else {
            return Ok(None);
        };
        let identity = CredentialIdentity {
            tenant_id: invocation.tenant_id(),
            connection_uid: invocation.action.connection_id.0,
            kind,
            slot_name: slot.clone(),
        };
        let context = CredentialContext {
            tenant_id: invocation.tenant_id(),
            principal: CredentialPrincipal::Caller {
                identity_id: invocation.caller.id,
                delegated_by: invocation.caller.acting_on_behalf_of,
            },
            operation: CredentialOperation::Resolve,
            operation_id: format!(
                "connector-http-resolve:{}:{}",
                invocation.tool_call_id, slot
            ),
            request_hash: request_hash.to_string(),
        };
        let secret = await_before_send(
            &invocation.cancellation_token,
            deadline,
            "credential_resolution",
            async {
                self.credential_vault
                    .resolve_active(&identity, &context)
                    .await
                    .map_err(Error::from)
            },
        )
        .await?;
        let value = credential_header_value(&secret, bearer)?;
        let name = HeaderName::from_bytes(header.as_bytes()).map_err(|_| Error::Http {
            code: "credential_header_rejected",
        })?;

        Ok(Some((name, value)))
    }
}

#[async_trait]
impl ConnectorActionRuntime for HttpConnectorRuntime {
    async fn invoke(
        &self,
        invocation: ConnectorActionInvocation,
        prepared: crate::executor::PreparedConnectorAction,
    ) -> Result<RawConnectorActionResult> {
        self.invoke_inner(invocation, prepared).await
    }
}

#[derive(Clone)]
enum SelectedAuth {
    None,
    Credential {
        slot: CredentialSlotName,
        kind: CredentialKind,
        header: String,
        bearer: bool,
    },
}

fn connection_origin(connection: &ConnectorConnection) -> Result<ConnectionOrigin> {
    connection
        .origin
        .clone()
        .ok_or(Error::InvalidConnectionOrigin {
            reason: "active constrained HTTP connection requires an origin",
        })
}

fn bounded_connect_timeout(
    contract: &HttpOperationContract,
    deadline: Instant,
) -> Result<Duration> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or(Error::Http {
            code: "total_timeout_before_send",
        })?;
    Ok(remaining.min(Duration::from_millis(u64::from(
        contract.connect_timeout_ms,
    ))))
}

async fn await_before_send<T, F>(
    cancellation: &CancellationToken,
    deadline: Instant,
    stage: &'static str,
    future: F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::select! {
        _ = cancellation.cancelled() => Err(Error::Cancelled { stage }),
        result = future => result,
        _ = tokio::time::sleep_until(deadline) => Err(Error::Http {
            code: "total_timeout_before_send",
        }),
    }
}

fn build_request_url(
    destination: &AdmittedHttpDestination,
    contract: &HttpOperationContract,
    input: &Value,
) -> Result<Url> {
    let mut path = contract.path_template.clone();
    for mapping in &contract.path_inputs {
        let value = input_scalar(input, &mapping.input_pointer)?;
        let placeholder = format!("{{{}}}", mapping.placeholder);
        path = path.replace(&placeholder, &percent_encode_path_segment(&value));
    }
    if path.contains(['{', '}']) {
        return Err(Error::Http {
            code: "path_mapping_rejected",
        });
    }

    let mut complete = Url::parse(&format!(
        "{}{}",
        destination
            .canonical_origin()
            .origin()
            .ascii_serialization(),
        path
    ))
    .map_err(|_| Error::Http {
        code: "request_url_rejected",
    })?;
    if !contract.query_inputs.is_empty() {
        let mut query = complete.query_pairs_mut();
        for mapping in &contract.query_inputs {
            query.append_pair(
                &mapping.parameter,
                &input_scalar(input, &mapping.input_pointer)?,
            );
        }
    }
    if complete.as_str().len() > MAX_COMPLETE_URL_BYTES
        || complete.origin() != destination.canonical_origin().origin()
    {
        return Err(Error::Http {
            code: "request_url_rejected",
        });
    }
    Ok(complete)
}

fn input_scalar(input: &Value, pointer: &str) -> Result<String> {
    let value = match input.pointer(pointer) {
        Some(Value::String(value)) if value.len() <= MAX_COMPLETE_URL_BYTES => value.clone(),
        Some(Value::Bool(value)) => value.to_string(),
        Some(Value::Number(value)) => value.to_string(),
        _ => {
            return Err(Error::Http {
                code: "input_mapping_rejected",
            });
        }
    };
    Ok(value)
}

fn percent_encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn build_request_body(contract: &HttpOperationContract, input: &Value) -> Result<Option<Vec<u8>>> {
    let Some(mapping) = &contract.body_input else {
        return Ok(None);
    };
    let body = input.pointer(&mapping.input_pointer).ok_or(Error::Http {
        code: "body_mapping_rejected",
    })?;
    let mut writer = LimitedBodyWriter::new(contract.max_request_bytes as usize);
    if let Err(error) = serde_json::to_writer(&mut writer, body) {
        if writer.exceeded {
            return Err(Error::Http {
                code: "request_body_too_large",
            });
        }
        return Err(Error::Serialization(error));
    }
    Ok(Some(writer.bytes))
}

struct LimitedBodyWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl LimitedBodyWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(8 * 1024)),
            limit,
            exceeded: false,
        }
    }
}

impl std::io::Write for LimitedBodyWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let within_limit = self
            .bytes
            .len()
            .checked_add(buffer.len())
            .is_some_and(|length| length <= self.limit);
        if !within_limit {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "connector request body limit exceeded",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn select_auth_requirement(
    binding: &InstalledActionBinding,
    contract: &HttpOperationContract,
) -> Result<SelectedAuth> {
    let Some(selected_slot) = &contract.credential_slot else {
        if binding.compiled_contract.auth.as_slice() == [RuntimeConnectorAuthRequirementV1::None] {
            return Ok(SelectedAuth::None);
        }
        return Err(Error::Http {
            code: "credential_contract_rejected",
        });
    };
    let requirement = binding
        .compiled_contract
        .auth
        .iter()
        .find(|requirement| requirement.slot() == Some(selected_slot))
        .ok_or(Error::Http {
            code: "credential_contract_rejected",
        })?;
    match requirement {
        RuntimeConnectorAuthRequirementV1::Bearer { slot } => Ok(SelectedAuth::Credential {
            slot: slot.clone(),
            kind: CredentialKind::ProviderApiKey,
            header: "authorization".to_string(),
            bearer: true,
        }),
        RuntimeConnectorAuthRequirementV1::ApiKeyHeader { slot, header } => {
            Ok(SelectedAuth::Credential {
                slot: slot.clone(),
                kind: CredentialKind::ProviderApiKey,
                header: header.as_str().to_string(),
                bearer: false,
            })
        }
        RuntimeConnectorAuthRequirementV1::ManagedOauth { slot } => Ok(SelectedAuth::Credential {
            slot: slot.clone(),
            kind: CredentialKind::OAuth,
            header: "authorization".to_string(),
            bearer: true,
        }),
        RuntimeConnectorAuthRequirementV1::None => Err(Error::Http {
            code: "credential_contract_rejected",
        }),
    }
}

fn credential_header_value(secret: &RedactedSecret, bearer: bool) -> Result<HeaderValue> {
    let exposed = secret.expose_for_outbound_request();
    if exposed.is_empty() {
        return Err(Error::Http {
            code: "credential_header_rejected",
        });
    }
    let plaintext = if bearer {
        Zeroizing::new(format!("Bearer {exposed}"))
    } else {
        Zeroizing::new(exposed.to_string())
    };
    if plaintext.len() > MAX_CREDENTIAL_HEADER_BYTES {
        return Err(Error::Http {
            code: "credential_header_rejected",
        });
    }
    let mut value = HeaderValue::from_bytes(plaintext.as_bytes()).map_err(|_| Error::Http {
        code: "credential_header_rejected",
    })?;
    value.set_sensitive(true);
    Ok(value)
}

fn build_client(
    destination: &AdmittedHttpDestination,
    contract: &HttpOperationContract,
    deadline: Instant,
) -> Result<reqwest::Client> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or(Error::Http {
            code: "total_timeout_before_send",
        })?;
    let limits = OutboundHttpClientLimits::new(
        remaining.min(Duration::from_millis(u64::from(
            contract.connect_timeout_ms,
        ))),
        remaining,
        MAX_RESPONSE_HEADER_BYTES as u32,
    )
    .map_err(|_| Error::Http {
        code: "transport_construction_failed",
    })?;
    build_admitted_http_client(destination, limits).map_err(|_| Error::Http {
        code: "transport_construction_failed",
    })
}

fn build_request(
    client: &reqwest::Client,
    contract: &HttpOperationContract,
    request_url: Url,
    request_body: Option<Vec<u8>>,
    credential_header: Option<(HeaderName, HeaderValue)>,
    upstream_idempotency_key: Option<String>,
) -> Result<reqwest::RequestBuilder> {
    let method = match contract.method {
        HttpMethodV1::Get => reqwest::Method::GET,
        HttpMethodV1::Post => reqwest::Method::POST,
        HttpMethodV1::Put => reqwest::Method::PUT,
        HttpMethodV1::Patch => reqwest::Method::PATCH,
        HttpMethodV1::Delete => reqwest::Method::DELETE,
    };
    let mut request = client.request(method, request_url);
    if let Some(body) = request_body {
        request = request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    if let Some((name, value)) = credential_header {
        request = request.header(name, value);
    }
    if let (Some(header), Some(value)) = (
        &contract.upstream_idempotency_header,
        upstream_idempotency_key,
    ) {
        let name = HeaderName::from_bytes(header.as_str().as_bytes()).map_err(|_| Error::Http {
            code: "idempotency_header_rejected",
        })?;
        let value = HeaderValue::from_str(&value).map_err(|_| Error::Http {
            code: "idempotency_header_rejected",
        })?;
        request = request.header(name, value);
    }
    Ok(request)
}

async fn read_response(
    response: reqwest::Response,
    contract: &HttpOperationContract,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<Value> {
    validate_response_head(&response, contract.max_response_bytes as usize)?;
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::select! {
            _ = cancellation.cancelled() => return Err(Error::Cancelled {
                stage: "response_body",
            }),
            next = stream.next() => next,
            _ = tokio::time::sleep_until(deadline) => return Err(Error::Http {
                code: "total_timeout_after_response",
            }),
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|error| Error::Http {
            code: if error.is_timeout() {
                "total_timeout_after_response"
            } else {
                "response_body_failed"
            },
        })?;
        let new_len = body.len().checked_add(chunk.len()).ok_or(Error::Http {
            code: "response_body_too_large",
        })?;
        if new_len > contract.max_response_bytes as usize {
            return Err(Error::Http {
                code: "response_body_too_large",
            });
        }
        body.extend_from_slice(&chunk);
    }
    let response: Value = serde_json::from_slice(&body).map_err(|_| Error::Http {
        code: "response_json_rejected",
    })?;
    let output = if let Some(pointer) = &contract.response_pointer {
        response.pointer(pointer).cloned().ok_or(Error::Http {
            code: "response_pointer_rejected",
        })?
    } else {
        response
    };
    Ok(output)
}

fn validate_response_head(response: &reqwest::Response, max_body_bytes: usize) -> Result<()> {
    let status = response.status();
    if status.is_redirection() {
        return Err(Error::Http {
            code: "redirect_rejected",
        });
    }
    if !status.is_success() {
        return Err(Error::Http {
            code: "upstream_status_rejected",
        });
    }
    let headers = response.headers();
    if headers.len() > MAX_RESPONSE_HEADER_COUNT
        || headers
            .iter()
            .try_fold(0_usize, |total, (name, value)| {
                total
                    .checked_add(name.as_str().len())?
                    .checked_add(value.as_bytes().len())
            })
            .is_none_or(|bytes| bytes > MAX_RESPONSE_HEADER_BYTES)
    {
        return Err(Error::Http {
            code: "response_headers_rejected",
        });
    }
    let mut content_types = headers.get_all(reqwest::header::CONTENT_TYPE).iter();
    let content_type = content_types
        .next()
        .filter(|_| content_types.next().is_none())
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .ok_or(Error::Http {
            code: "response_content_type_rejected",
        })?;
    let json_type = content_type == "application/json"
        || (content_type.starts_with("application/") && content_type.ends_with("+json"));
    if !json_type {
        return Err(Error::Http {
            code: "response_content_type_rejected",
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_body_bytes as u64)
    {
        return Err(Error::Http {
            code: "response_body_too_large",
        });
    }
    Ok(())
}
