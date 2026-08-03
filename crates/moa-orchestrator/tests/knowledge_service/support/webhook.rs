// Provider and parser webhook request/verifier fixtures.

fn webhook_request(
    tenant_id: TenantId,
    connection_uid: Uuid,
    event_id: &str,
) -> KnowledgeProviderWebhookRequest {
    let payload = json!({
        "tenant_id": tenant_id.to_string(),
        "connection_uid": connection_uid.to_string(),
        "event_id": event_id,
        "event_type": "sync.completed"
    });
    KnowledgeProviderWebhookRequest {
        provider: PROVIDER.to_string(),
        event_id: event_id.to_string(),
        event_type: "sync.completed".to_string(),
        payload,
        headers: vec![("x-test-signature".to_string(), "valid".to_string())],
        body_base64: None,
    }
}

fn signed_connection_webhook_request(
    provider: &str,
    tenant_id: TenantId,
    connection_uid: Uuid,
    event_id: &str,
    event_type: &str,
) -> KnowledgeProviderWebhookRequest {
    signed_provider_webhook_request(
        provider,
        json!({
            "tenant_id": tenant_id.to_string(),
            "connection_uid": connection_uid.to_string(),
            "event_id": event_id,
            "event_type": event_type
        }),
    )
}

fn signed_provider_webhook_request(
    provider: &str,
    payload: Value,
) -> KnowledgeProviderWebhookRequest {
    let event_id = payload
        .get("event_id")
        .and_then(Value::as_str)
        .expect("provider webhook fixture should include event_id")
        .to_string();
    let event_type = payload
        .get("event_type")
        .and_then(Value::as_str)
        .expect("provider webhook fixture should include event_type")
        .to_string();
    KnowledgeProviderWebhookRequest {
        provider: provider.to_string(),
        event_id,
        event_type,
        payload,
        headers: vec![("x-test-signature".to_string(), "valid".to_string())],
        body_base64: None,
    }
}

fn parser_webhook_payload(
    tenant_id: TenantId,
    connection_uid: Uuid,
    object_uid: Option<Uuid>,
    source_id: Option<&str>,
    event_id: &str,
) -> Value {
    let mut payload = json!({
        "tenant_id": tenant_id.to_string(),
        "connection_uid": connection_uid.to_string(),
        "event_id": event_id,
        "event_type": "parse.completed",
        "status": "completed",
        "metadata": {
            "safe": "parser",
            "access_token": SECRET_TOKEN,
            "raw_document_text": format!("parser document body {RAW_DOCUMENT_TAIL}")
        }
    });
    if let Some(object_uid) = object_uid {
        payload["object_uid"] = json!(object_uid.to_string());
    }
    if let Some(source_id) = source_id {
        payload["source_id"] = json!(source_id);
    }
    payload
}

fn parser_webhook_request(
    provider: &str,
    payload: Value,
    headers: Vec<(String, String)>,
) -> KnowledgeProviderWebhookRequest {
    let event_id = payload
        .get("event_id")
        .and_then(Value::as_str)
        .expect("parser webhook fixture should include event_id")
        .to_string();
    let event_type = payload
        .get("event_type")
        .and_then(Value::as_str)
        .expect("parser webhook fixture should include event_type")
        .to_string();
    KnowledgeProviderWebhookRequest {
        provider: provider.to_string(),
        event_id,
        event_type,
        payload,
        headers,
        body_base64: None,
    }
}

fn webhook_signature_hex(signing_key: &str, payload: &Value) -> String {
    let body = serde_json::to_vec(payload).expect("parser webhook fixture should serialize");
    let mut mac = Hmac::<Sha256>::new_from_slice(signing_key.as_bytes())
        .expect("parser webhook signing key should be valid");
    mac.update(&body);
    hex::encode(mac.finalize().into_bytes())
}

#[derive(Debug, Clone)]
struct PayloadWebhookVerifier {
    provider: &'static str,
}

impl PayloadWebhookVerifier {
    fn new(provider: &'static str) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl KnowledgeWebhookVerifier for PayloadWebhookVerifier {
    async fn verify_webhook(
        &self,
        _headers: HeaderMap,
        body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent> {
        let value: Value = serde_json::from_slice(&body)
            .map_err(|error| KnowledgeError::provider(self.provider, error.to_string()))?;
        let event_id = value
            .get("event_id")
            .and_then(Value::as_str)
            .ok_or_else(|| KnowledgeError::provider(self.provider, "missing `event_id`"))?;
        let event_type = value
            .get("event_type")
            .and_then(Value::as_str)
            .ok_or_else(|| KnowledgeError::provider(self.provider, "missing `event_type`"))?;
        Ok(WebhookEvent {
            provider: self.provider.to_string(),
            event_id: event_id.to_string(),
            event_type: event_type.to_string(),
            metadata: value,
        })
    }
}

#[derive(Debug, Clone)]
struct FixedWebhookVerifier {
    event: WebhookEvent,
}

impl FixedWebhookVerifier {
    fn new(event: WebhookEvent) -> Self {
        Self { event }
    }
}

#[async_trait]
impl KnowledgeWebhookVerifier for FixedWebhookVerifier {
    async fn verify_webhook(
        &self,
        _headers: HeaderMap,
        _body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent> {
        Ok(self.event.clone())
    }
}
