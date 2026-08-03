// Linked-provider fakes and provider-record catalogs.

#[derive(Debug, Clone)]
struct Task14LinkedIntegrationProvider {
    provider: &'static str,
    connector: &'static str,
    records: Arc<Vec<ProviderRecord>>,
    calls: Arc<Mutex<FakeProviderCalls>>,
    list_error: Option<&'static str>,
}

impl Task14LinkedIntegrationProvider {
    fn new(provider: &'static str, connector: &'static str, records: Vec<ProviderRecord>) -> Self {
        Self {
            provider,
            connector,
            records: Arc::new(records),
            calls: Arc::new(Mutex::new(FakeProviderCalls::default())),
            list_error: None,
        }
    }

    fn failing_list(
        provider: &'static str,
        connector: &'static str,
        message: &'static str,
    ) -> Self {
        Self {
            provider,
            connector,
            records: Arc::new(Vec::new()),
            calls: Arc::new(Mutex::new(FakeProviderCalls::default())),
            list_error: Some(message),
        }
    }

    fn trigger_sync_count(&self) -> usize {
        self.calls().trigger_sync
    }

    fn start_initial_sync_count(&self) -> usize {
        self.calls().start_initial_sync
    }

    fn list_changed_records_count(&self) -> usize {
        self.calls().list_changed_records
    }

    fn list_changed_record_requests(&self) -> Vec<FakeListChangedRecordsRequest> {
        self.calls().list_changed_record_requests
    }

    fn calls(&self) -> FakeProviderCalls {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .clone()
    }
}

#[async_trait]
impl LinkedIntegrationProvider for Task14LinkedIntegrationProvider {
    async fn create_link_token(
        &self,
        _req: CreateLinkTokenRequest,
    ) -> moa_knowledge::Result<LinkToken> {
        Ok(LinkToken {
            provider: linked_provider(self.provider),
            token: format!("{}-task14-link-token", self.provider),
            link_url: Some(format!("https://{}.example.test/link", self.provider)),
            expires_at: None,
        })
    }

    async fn exchange_public_token(
        &self,
        _req: ExchangePublicTokenRequest,
    ) -> moa_knowledge::Result<LinkedAccount> {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .exchange_public_token += 1;
        Ok(LinkedAccount {
            provider: linked_provider(self.provider),
            connector: self.connector.to_string(),
            provider_account_id: format!("{}-task14-account", self.provider),
            credential_material: (self.provider == "merge")
                .then(|| format!("{}-raw-token-should-enter-vault", self.provider)),
            metadata: json!({
                "provider": self.provider,
                "access_token": format!("{}-secret", self.provider),
            }),
        })
    }

    async fn trigger_sync(&self, req: TriggerSyncRequest) -> moa_knowledge::Result<TriggeredSync> {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .trigger_sync += 1;
        Ok(TriggeredSync {
            provider: linked_provider(self.provider),
            provider_sync_id: Some(format!(
                "{}-sync-{}",
                self.provider, req.connection.connection_uid
            )),
            status: "accepted".to_string(),
            metadata: json!({ "provider_trigger": "accepted" }),
        })
    }

    async fn start_initial_sync(
        &self,
        req: StartInitialSyncRequest,
    ) -> moa_knowledge::Result<InitialSyncStarted> {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .start_initial_sync += 1;
        Ok(InitialSyncStarted {
            provider: linked_provider(self.provider),
            provider_sync_id: Some(format!(
                "{}-initial-{}",
                self.provider, req.connection.connection_uid
            )),
            completed: false,
            metadata: json!({ "initial_sync": "started" }),
        })
    }

    async fn revoke_remote_connection(
        &self,
        _req: RemoteRevokeRequest,
    ) -> moa_knowledge::Result<()> {
        Ok(())
    }

    async fn list_changed_records(
        &self,
        req: ListChangedRecordsRequest,
    ) -> moa_knowledge::Result<RecordPage> {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .record_list_changed_records_request(&req);
        if let Some(message) = self.list_error {
            return Err(KnowledgeError::provider(self.provider, message));
        }
        let limit = req.limit.unwrap_or(u32::MAX) as usize;
        Ok(RecordPage {
            records: self.records.iter().take(limit).cloned().collect(),
            next_cursor: None,
        })
    }

    async fn verify_webhook(
        &self,
        _headers: HeaderMap,
        _body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent> {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .verify_webhook += 1;
        Ok(WebhookEvent {
            provider: self.provider.to_string(),
            event_id: format!("{}-task14-webhook", self.provider),
            event_type: "sync.completed".to_string(),
            metadata: json!({ "provider": self.provider }),
        })
    }
}

#[derive(Debug, Clone)]
struct FakeLinkedIntegrationProvider {
    calls: Arc<Mutex<FakeProviderCalls>>,
    trigger_status: String,
    integrations: Vec<ProviderIntegration>,
    integrations_error: Option<String>,
    initial_sync_error: Option<String>,
    remote_revoke_error: Option<String>,
    provider: &'static str,
    credential_material: Option<String>,
}

impl Default for FakeLinkedIntegrationProvider {
    fn default() -> Self {
        Self {
            calls: Arc::new(Mutex::new(FakeProviderCalls::default())),
            trigger_status: "accepted".to_string(),
            integrations: Vec::new(),
            integrations_error: None,
            initial_sync_error: None,
            remote_revoke_error: None,
            provider: PROVIDER,
            credential_material: Some(SECRET_TOKEN.to_string()),
        }
    }
}

impl FakeLinkedIntegrationProvider {
    fn with_trigger_status(status: impl Into<String>) -> Self {
        Self {
            trigger_status: status.into(),
            ..Self::default()
        }
    }

    fn with_integrations(integrations: Vec<ProviderIntegration>) -> Self {
        Self {
            integrations,
            ..Self::default()
        }
    }

    fn with_integrations_error(message: impl Into<String>) -> Self {
        Self {
            integrations_error: Some(message.into()),
            ..Self::default()
        }
    }

    /// Fails only the initial-link sync start, leaving link steps before it intact.
    fn with_initial_sync_error(message: impl Into<String>) -> Self {
        Self {
            initial_sync_error: Some(message.into()),
            ..Self::default()
        }
    }

    fn with_remote_revoke_error(message: impl Into<String>) -> Self {
        Self {
            remote_revoke_error: Some(message.into()),
            ..Self::default()
        }
    }

    fn nango_with_initial_sync_error(message: impl Into<String>) -> Self {
        Self {
            provider: "nango",
            credential_material: None,
            initial_sync_error: Some(message.into()),
            ..Self::default()
        }
    }

    fn trigger_sync_count(&self) -> usize {
        self.calls().trigger_sync
    }

    fn start_initial_sync_count(&self) -> usize {
        self.calls().start_initial_sync
    }

    fn list_changed_records_count(&self) -> usize {
        self.calls().list_changed_records
    }

    fn exchange_count(&self) -> usize {
        self.calls().exchange_public_token
    }

    fn apply_source_selection_count(&self) -> usize {
        self.calls().apply_source_selection
    }

    fn remote_revoke_count(&self) -> usize {
        self.calls().remote_revoke
    }

    fn applied_source_selections(&self) -> Vec<Value> {
        self.calls().source_selection_requests
    }

    fn calls(&self) -> FakeProviderCalls {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .clone()
    }
}

#[derive(Debug, Clone, Default)]
struct FakeProviderCalls {
    exchange_public_token: usize,
    apply_source_selection: usize,
    trigger_sync: usize,
    start_initial_sync: usize,
    list_changed_records: usize,
    verify_webhook: usize,
    remote_revoke: usize,
    list_changed_record_requests: Vec<FakeListChangedRecordsRequest>,
    source_selection_requests: Vec<Value>,
}

impl FakeProviderCalls {
    fn record_list_changed_records_request(&mut self, req: &ListChangedRecordsRequest) {
        self.list_changed_records += 1;
        self.list_changed_record_requests
            .push(FakeListChangedRecordsRequest {
                connection_uid: req.connection.connection_uid,
                cursor: req.cursor.clone(),
                limit: req.limit,
                modified_after: req.modified_after,
                variant: req.variant.clone(),
            });
    }
}

#[async_trait]
impl LinkedIntegrationProvider for FakeLinkedIntegrationProvider {
    async fn list_integrations(&self) -> moa_knowledge::Result<Vec<ProviderIntegration>> {
        if let Some(message) = &self.integrations_error {
            return Err(KnowledgeError::Provider {
                provider: self.provider.to_string(),
                message: message.clone(),
            });
        }
        Ok(self.integrations.clone())
    }

    async fn create_link_token(
        &self,
        _req: CreateLinkTokenRequest,
    ) -> moa_knowledge::Result<LinkToken> {
        Ok(LinkToken {
            provider: linked_provider(self.provider),
            token: "link-token".to_string(),
            link_url: Some("https://provider.example/link".to_string()),
            expires_at: None,
        })
    }

    async fn exchange_public_token(
        &self,
        _req: ExchangePublicTokenRequest,
    ) -> moa_knowledge::Result<LinkedAccount> {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .exchange_public_token += 1;
        Ok(LinkedAccount {
            provider: linked_provider(self.provider),
            connector: CONNECTOR.to_string(),
            provider_account_id: "provider-account-1".to_string(),
            credential_material: self.credential_material.clone(),
            metadata: json!({
                "safe": "account",
                "access_token": SECRET_TOKEN
            }),
        })
    }

    async fn trigger_sync(&self, req: TriggerSyncRequest) -> moa_knowledge::Result<TriggeredSync> {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .trigger_sync += 1;
        Ok(TriggeredSync {
            provider: linked_provider(self.provider),
            provider_sync_id: Some(format!("sync-{}", req.connection.connection_uid)),
            status: self.trigger_status.clone(),
            metadata: json!({ "status": self.trigger_status.clone() }),
        })
    }

    async fn start_initial_sync(
        &self,
        req: StartInitialSyncRequest,
    ) -> moa_knowledge::Result<InitialSyncStarted> {
        let mut calls = self
            .calls
            .lock()
            .expect("fake provider call log should not be poisoned");
        calls.start_initial_sync += 1;
        if let Some(message) = self.initial_sync_error.clone() {
            return Err(KnowledgeError::provider(self.provider, message));
        }
        Ok(InitialSyncStarted {
            provider: linked_provider(self.provider),
            provider_sync_id: Some(format!("initial-{}", req.connection.connection_uid)),
            completed: provider_status_is_completed(&self.trigger_status),
            metadata: json!({ "status": self.trigger_status.clone() }),
        })
    }

    async fn revoke_remote_connection(
        &self,
        _req: RemoteRevokeRequest,
    ) -> moa_knowledge::Result<()> {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .remote_revoke += 1;
        if let Some(message) = &self.remote_revoke_error {
            return Err(KnowledgeError::provider(self.provider, message));
        }
        Ok(())
    }

    async fn apply_source_selection(
        &self,
        req: ApplySourceSelectionRequest,
    ) -> moa_knowledge::Result<()> {
        let mut calls = self
            .calls
            .lock()
            .expect("fake provider call log should not be poisoned");
        calls.apply_source_selection += 1;
        calls
            .source_selection_requests
            .push(req.connection.source_selection);
        Ok(())
    }

    async fn list_changed_records(
        &self,
        req: ListChangedRecordsRequest,
    ) -> moa_knowledge::Result<RecordPage> {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .record_list_changed_records_request(&req);
        Ok(RecordPage {
            records: Vec::new(),
            next_cursor: None,
        })
    }

    async fn verify_webhook(
        &self,
        _headers: HeaderMap,
        body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent> {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .verify_webhook += 1;
        let value: Value = serde_json::from_slice(&body)
            .map_err(|error| KnowledgeError::provider(self.provider, error.to_string()))?;
        Ok(WebhookEvent {
            provider: self.provider.to_string(),
            event_id: required_string(&value, "event_id")?,
            event_type: required_string(&value, "event_type")?,
            metadata: value,
        })
    }
}

fn required_string(value: &Value, field: &str) -> moa_knowledge::Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| KnowledgeError::provider(PROVIDER, format!("missing `{field}`")))
}
