// In-memory implementations of the six knowledge repository ports.

#[derive(Debug, Clone, Default)]
struct InMemoryKnowledgeRepository {
    state: Arc<Mutex<RepositoryState>>,
}

impl InMemoryKnowledgeRepository {
    fn insert_connection(&self, connection: KnowledgeConnection) -> moa_knowledge::Result<()> {
        self.with_state(|state| {
            state
                .connections
                .insert(connection.connection_uid, connection);
        })
    }

    fn insert_object_inspection(
        &self,
        object: KnowledgeObject,
        version: DocumentVersion,
        chunks: Vec<KnowledgeChunk>,
    ) -> moa_knowledge::Result<()> {
        self.with_state(|state| {
            state.versions.insert(version.object_uid, version.clone());
            state.chunks.insert(version.version_uid, chunks);
            state.objects.insert(object.object_uid, object);
        })
    }

    fn connection(&self, connection_uid: Uuid) -> Option<KnowledgeConnection> {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .connections
            .get(&connection_uid)
            .cloned()
    }

    fn disconnect_progress(
        &self,
        connection_uid: Uuid,
    ) -> Option<KnowledgeConnectionDisconnectProgress> {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .disconnects
            .get(&connection_uid)
            .cloned()
    }

    fn op_count(&self, op: &'static str) -> usize {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .op_counts
            .get(op)
            .copied()
            .unwrap_or(0)
    }

    /// Returns the only link claim recorded, failing when there is not exactly one.
    fn only_link_claim(&self) -> LinkClaim {
        let state = self
            .state
            .lock()
            .expect("repository state should not be poisoned");
        assert_eq!(
            state.link_claims.len(),
            1,
            "expected exactly one link claim, found {}",
            state.link_claims.len()
        );
        state
            .link_claims
            .values()
            .next()
            .cloned()
            .expect("link claim should be present")
    }

    /// Marks one sync run completed so the connection's active slot is free.
    fn finish_sync_run(&self, sync_run_uid: Uuid) {
        let mut state = self
            .state
            .lock()
            .expect("repository state should not be poisoned");
        let run = state
            .sync_runs
            .get_mut(&sync_run_uid)
            .expect("sync run should exist");
        run.status = SyncRunStatus::Completed;
        run.finished_at = Some(moa_test_support::fixtures::pg_now());
    }

    /// Returns every recorded link claim state.
    fn link_claim_states(&self) -> Vec<LinkClaimState> {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .link_claims
            .values()
            .map(|claim| claim.state)
            .collect()
    }

    /// Replaces one recorded claim, used to model a divergent replay.
    fn overwrite_link_claim(&self, claim: LinkClaim) {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .link_claims
            .insert((claim.tenant_id, claim.operation_id.clone()), claim);
    }

    /// Rewinds every finalized claim to `credential_written`, modelling a crash
    /// after the credential exists but before the link finalized.
    fn rewind_link_claim_to_credential_written(&self) {
        let mut state = self
            .state
            .lock()
            .expect("repository state should not be poisoned");
        for claim in state.link_claims.values_mut() {
            claim.state = LinkClaimState::CredentialWritten;
        }
    }

    /// Clears a claimed run's durable trigger boundary, modelling a crash between
    /// the durable sync-run claim and the provider dispatch.
    fn clear_provider_trigger_boundary(&self, sync_run_uid: Uuid) {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .sync_runs
            .get_mut(&sync_run_uid)
            .expect("sync run should exist")
            .provider_trigger_completed_at = None;
    }

    fn sync_run_count(&self) -> usize {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .sync_runs
            .len()
    }

    fn sync_run(&self, sync_run_uid: Uuid) -> Option<KnowledgeSyncRun> {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .sync_runs
            .get(&sync_run_uid)
            .cloned()
    }

    fn step_count(&self) -> usize {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .steps
            .len()
    }

    fn provider_event_count(&self) -> usize {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .provider_events
            .len()
    }

    fn provider_event(
        &self,
        tenant_id: TenantId,
        provider: &str,
        provider_event_id: &str,
    ) -> Option<KnowledgeProviderEventRecord> {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .provider_events
            .get(&(
                tenant_id,
                provider.to_string(),
                provider_event_id.to_string(),
            ))
            .cloned()
    }

    fn record_op(&self, op: &'static str) -> moa_knowledge::Result<()> {
        self.with_state(|state| {
            *state.op_counts.entry(op).or_insert(0) += 1;
        })
    }

    fn with_state<T>(
        &self,
        apply: impl FnOnce(&mut RepositoryState) -> T,
    ) -> moa_knowledge::Result<T> {
        self.state
            .lock()
            .map_err(|error| {
                KnowledgeError::Repository(format!("repository mutex poisoned: {error}"))
            })
            .map(|mut state| apply(&mut state))
    }
}

#[derive(Debug, Default)]
struct RepositoryState {
    connections: HashMap<Uuid, KnowledgeConnection>,
    sync_runs: HashMap<Uuid, KnowledgeSyncRun>,
    steps: Vec<KnowledgeIngestionStep>,
    objects: HashMap<Uuid, KnowledgeObject>,
    versions: HashMap<Uuid, DocumentVersion>,
    ingestion_claims: HashMap<(Uuid, String), InMemoryDocumentIngestionClaim>,
    chunks: HashMap<Uuid, Vec<KnowledgeChunk>>,
    provider_events: HashMap<(TenantId, String, String), KnowledgeProviderEventRecord>,
    link_claims: HashMap<(TenantId, String), LinkClaim>,
    disconnects: HashMap<Uuid, KnowledgeConnectionDisconnectProgress>,
    /// Stored snapshots keyed by uid, mirroring the immutable SQL table: an
    /// entry set is inserted once and never edited in place.
    acl_snapshots: HashMap<Uuid, moa_knowledge::domain::ProviderAclSnapshot>,
    acl_bindings: Vec<moa_knowledge::domain::SourcePrincipalBinding>,
    acl_group_bindings: Vec<moa_knowledge::domain::SourcePrincipalGroupBinding>,
    op_counts: HashMap<&'static str, usize>,
}

#[derive(Debug, Clone)]
struct InMemoryDocumentIngestionClaim {
    version: DocumentVersion,
    sync_run_uid: Uuid,
    claim_token: Uuid,
    status: InMemoryDocumentIngestionClaimStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InMemoryDocumentIngestionClaimStatus {
    Started,
    Completed,
    Failed,
}

fn sync_run_is_active(status: SyncRunStatus) -> bool {
    matches!(
        status,
        SyncRunStatus::Queued
            | SyncRunStatus::ProviderSyncing
            | SyncRunStatus::ProviderSynced
            | SyncRunStatus::ParsePending
            | SyncRunStatus::Ingesting
    )
}

#[async_trait]
impl KnowledgeDiscoveryStore for InMemoryKnowledgeRepository {
    async fn lookup_connection_by_provider_account(
        &self,
        provider: moa_knowledge::domain::LinkedProviderKind,
        connector: Option<&str>,
        provider_account_id: &str,
    ) -> moa_knowledge::Result<ProviderAccountConnectionLookup> {
        self.record_op("lookup_connection_by_provider_account")?;
        self.with_state(|state| {
            let matches = state
                .connections
                .values()
                .filter(|connection| connection.provider == provider)
                .filter(|connection| {
                    connector.is_none_or(|connector| connector == connection.connector)
                })
                .filter(|connection| connection.provider_account_id == provider_account_id)
                .take(2)
                .cloned()
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [] => ProviderAccountConnectionLookup::NotFound,
                [connection] => ProviderAccountConnectionLookup::Unique(connection.clone()),
                matches => ProviderAccountConnectionLookup::Ambiguous {
                    matches: matches.len(),
                },
            }
        })
    }

    async fn resolve_sync_run_tenant(
        &self,
        sync_run_uid: Uuid,
    ) -> moa_knowledge::Result<Option<TenantId>> {
        self.record_op("resolve_sync_run_tenant")?;
        self.with_state(|state| state.sync_runs.get(&sync_run_uid).map(|run| run.tenant_id))
    }
}

#[async_trait]
impl KnowledgeConnectionRepository for InMemoryKnowledgeRepository {
    async fn upsert_connection(
        &self,
        connection: KnowledgeConnection,
    ) -> moa_knowledge::Result<KnowledgeConnection> {
        self.record_op("upsert_connection")?;
        self.with_state(|state| {
            // Mirrors the Postgres conflict target
            // `(tenant_id, provider, provider_config_key, provider_connection_id)`:
            // re-linking the same provider account keeps the existing
            // connection identifier rather than adopting the caller's.
            let existing = state
                .connections
                .values()
                .find(|candidate| {
                    candidate.tenant_id == connection.tenant_id
                        && candidate.provider == connection.provider
                        && candidate.connector == connection.connector
                        && candidate.provider_account_id == connection.provider_account_id
                })
                .map(|candidate| candidate.connection_uid);
            let mut stored = connection;
            if let Some(connection_uid) = existing {
                stored.connection_uid = connection_uid;
            }
            state
                .connections
                .insert(stored.connection_uid, stored.clone());
            stored
        })
    }

    async fn get_connection(
        &self,
        connection_uid: Uuid,
    ) -> moa_knowledge::Result<Option<KnowledgeConnection>> {
        self.record_op("get_connection")?;
        self.with_state(|state| state.connections.get(&connection_uid).cloned())
    }

    async fn mark_connection_synced(
        &self,
        connection_uid: Uuid,
        completed_at: DateTime<Utc>,
    ) -> moa_knowledge::Result<()> {
        self.record_op("mark_connection_synced")?;
        self.with_state(|state| {
            let connection = state.connections.get_mut(&connection_uid).ok_or_else(|| {
                KnowledgeError::Repository(
                    "active knowledge connection was not visible for sync completion".to_string(),
                )
            })?;
            connection.last_synced_at = Some(completed_at);
            connection.updated_at = completed_at;
            Ok(())
        })?
    }

    async fn connection_by_provider_account(
        &self,
        provider: moa_knowledge::domain::LinkedProviderKind,
        connector: &str,
        provider_account_id: &str,
    ) -> moa_knowledge::Result<Option<KnowledgeConnection>> {
        self.record_op("connection_by_provider_account")?;
        self.with_state(|state| {
            state
                .connections
                .values()
                .find(|candidate| {
                    candidate.provider == provider
                        && candidate.connector == connector
                        && candidate.provider_account_id == provider_account_id
                })
                .cloned()
        })
    }

    async fn reserve_link_claim(
        &self,
        claim: NewLinkClaim,
    ) -> moa_knowledge::Result<LinkClaimReservation> {
        self.record_op("reserve_link_claim")?;
        self.with_state(|state| {
            let key = (claim.tenant_id, claim.operation_id.clone());
            if let Some(existing) = state.link_claims.get(&key) {
                if existing.request_hash != claim.request_hash
                    || existing.connection_uid != claim.connection_uid
                {
                    return LinkClaimReservation::Conflict;
                }
                return LinkClaimReservation::Existing(existing.clone());
            }
            if state.link_claims.values().any(|existing| {
                existing.tenant_id == claim.tenant_id
                    && existing.connection_uid == claim.connection_uid
                    && !matches!(
                        existing.state,
                        LinkClaimState::Finalized | LinkClaimState::Compensated
                    )
            }) {
                return LinkClaimReservation::ConnectionBusy;
            }
            if claim.owner_identity_id.is_none() {
                return LinkClaimReservation::OwnerRequired;
            }
            let now = moa_test_support::fixtures::pg_now();
            let reserved = LinkClaim {
                tenant_id: claim.tenant_id,
                operation_id: claim.operation_id,
                request_hash: claim.request_hash,
                owner_identity_id: claim.owner_identity_id,
                connection_uid: claim.connection_uid,
                parent_created_by_claim: false,
                credential_expected_generation: None,
                credential_ownership: None,
                candidate_credential_ref: None,
                previous_vault_credential_ref: None,
                state: LinkClaimState::Reserved,
                sync_run_uid: None,
                created_at: now,
                updated_at: now,
            };
            state.link_claims.insert(key, reserved.clone());
            LinkClaimReservation::Reserved(reserved)
        })
    }

    async fn advance_link_claim(
        &self,
        tenant_id: TenantId,
        operation_id: &str,
        transition: LinkClaimTransition,
    ) -> moa_knowledge::Result<Option<LinkClaim>> {
        self.record_op("advance_link_claim")?;
        self.with_state(|state| {
            let claim = state
                .link_claims
                .get_mut(&(tenant_id, operation_id.to_string()))?;
            if !transition.permitted_source_states().contains(&claim.state) {
                return None;
            }
            match &transition {
                LinkClaimTransition::ParentClaimed {
                    parent_created_by_claim,
                    credential_expected_generation,
                } => {
                    claim.parent_created_by_claim = *parent_created_by_claim;
                    claim.credential_expected_generation = Some(*credential_expected_generation);
                }
                LinkClaimTransition::CredentialWritten {
                    credential_ownership,
                    candidate_credential_ref,
                    previous_vault_credential_ref,
                } => {
                    claim.credential_ownership = Some(*credential_ownership);
                    claim.candidate_credential_ref = candidate_credential_ref.clone();
                    claim.previous_vault_credential_ref = previous_vault_credential_ref.clone();
                }
                LinkClaimTransition::SyncRunClaimed { sync_run_uid }
                | LinkClaimTransition::Finalized { sync_run_uid } => {
                    claim.sync_run_uid = Some(*sync_run_uid);
                }
                LinkClaimTransition::Compensating | LinkClaimTransition::Compensated => {}
            }
            claim.state = transition.target_state();
            claim.updated_at = moa_test_support::fixtures::pg_now();
            Some(claim.clone())
        })
    }

    async fn get_link_claim(
        &self,
        tenant_id: TenantId,
        operation_id: &str,
    ) -> moa_knowledge::Result<Option<LinkClaim>> {
        self.record_op("get_link_claim")?;
        self.with_state(|state| {
            state
                .link_claims
                .get(&(tenant_id, operation_id.to_string()))
                .cloned()
        })
    }

    async fn reserve_connection_disconnect(
        &self,
        disconnect: NewKnowledgeConnectionDisconnect,
    ) -> moa_knowledge::Result<KnowledgeDisconnectReservation> {
        self.record_op("reserve_connection_disconnect")?;
        self.with_state(|state| {
            if let Some(existing) = state.disconnects.get(&disconnect.connection_uid) {
                if existing.tenant_id != disconnect.tenant_id
                    || existing.request_hash != disconnect.request_hash
                {
                    return KnowledgeDisconnectReservation::OperationConflict;
                }
                return KnowledgeDisconnectReservation::Existing(existing.clone());
            }
            if state.disconnects.values().any(|existing| {
                existing.tenant_id == disconnect.tenant_id
                    && existing.operation_id == disconnect.operation_id
            }) {
                return KnowledgeDisconnectReservation::OperationConflict;
            }
            let now = moa_test_support::fixtures::pg_now();
            let progress = KnowledgeConnectionDisconnectProgress {
                tenant_id: disconnect.tenant_id,
                connection_uid: disconnect.connection_uid,
                operation_id: disconnect.operation_id,
                request_hash: disconnect.request_hash,
                provider_operation_id: disconnect.provider_operation_id,
                state: KnowledgeDisconnectState::Reserved,
                error_code: None,
                created_at: now,
                updated_at: now,
                completed_at: None,
            };
            state
                .disconnects
                .insert(progress.connection_uid, progress.clone());
            KnowledgeDisconnectReservation::Reserved(progress)
        })
    }

    async fn advance_connection_disconnect(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
        transition: KnowledgeDisconnectTransition,
    ) -> moa_knowledge::Result<Option<KnowledgeConnectionDisconnectProgress>> {
        self.record_op("advance_connection_disconnect")?;
        self.with_state(|state| {
            let progress = state.disconnects.get_mut(&connection_uid)?;
            if progress.tenant_id != tenant_id || progress.state != transition.source_state() {
                return None;
            }
            progress.state = transition.target_state();
            progress.error_code = transition.error_code().map(ToOwned::to_owned);
            progress.updated_at = moa_test_support::fixtures::pg_now();
            if progress.state.is_terminal() {
                progress.completed_at = Some(progress.updated_at);
            }
            Some(progress.clone())
        })
    }

    async fn get_connection_disconnect(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
    ) -> moa_knowledge::Result<Option<KnowledgeConnectionDisconnectProgress>> {
        self.record_op("get_connection_disconnect")?;
        self.with_state(|state| {
            state
                .disconnects
                .get(&connection_uid)
                .filter(|progress| progress.tenant_id == tenant_id)
                .cloned()
        })
    }

    async fn delete_connection_projection(
        &self,
        connection_uid: Uuid,
    ) -> moa_knowledge::Result<bool> {
        self.record_op("delete_connection_projection")?;
        self.with_state(|state| state.connections.remove(&connection_uid).is_some())
    }

    async fn purge_tenant_link_claims(&self, limit: u32) -> moa_knowledge::Result<u64> {
        self.record_op("purge_tenant_link_claims")?;
        self.with_state(|state| {
            let mut keys: Vec<(TenantId, String)> = state.link_claims.keys().cloned().collect();
            keys.sort_by(|left, right| (left.0.0, &left.1).cmp(&(right.0.0, &right.1)));
            keys.truncate(limit.max(1) as usize);
            for key in &keys {
                state.link_claims.remove(key);
            }
            keys.len() as u64
        })
    }

    async fn update_connection_source_selection(
        &self,
        connection_uid: Uuid,
        source_selection: Value,
    ) -> moa_knowledge::Result<KnowledgeConnection> {
        self.record_op("update_connection_source_selection")?;
        self.with_state(|state| {
            let connection = state.connections.get_mut(&connection_uid).ok_or_else(|| {
                KnowledgeError::Repository("connection should exist for fixture update".to_string())
            })?;
            connection.source_selection = source_selection;
            connection.last_synced_at = None;
            connection.updated_at = moa_test_support::fixtures::pg_now();
            Ok(connection.clone())
        })?
    }

    async fn list_connections(
        &self,
        tenant_id: TenantId,
        provider: Option<moa_knowledge::domain::LinkedProviderKind>,
    ) -> moa_knowledge::Result<Vec<KnowledgeConnectionProjection>> {
        self.record_op("list_connections")?;
        self.with_state(|state| {
            state
                .connections
                .values()
                .filter(|connection| connection.tenant_id == tenant_id)
                .filter(|connection| {
                    provider.is_none_or(|provider| provider == connection.provider)
                })
                .cloned()
                .map(|connection| {
                    let last_sync_status = state
                        .sync_runs
                        .values()
                        .filter(|run| run.connection_uid == connection.connection_uid)
                        .max_by_key(|run| run.started_at)
                        .map(|run| run.status);
                    KnowledgeConnectionProjection {
                        connection,
                        parent_lifecycle_status: "active".to_string(),
                        last_sync_status,
                    }
                })
                .collect()
        })
    }
}

#[async_trait]
impl KnowledgeSyncRepository for InMemoryKnowledgeRepository {
    async fn mark_provider_trigger_completed(
        &self,
        sync_run_uid: Uuid,
    ) -> moa_knowledge::Result<()> {
        self.record_op("mark_provider_trigger_completed")?;
        self.with_state(|state| {
            if let Some(run) = state.sync_runs.get_mut(&sync_run_uid) {
                // Write-once, matching the Postgres `COALESCE`.
                run.provider_trigger_completed_at = run
                    .provider_trigger_completed_at
                    .or_else(|| Some(moa_test_support::fixtures::pg_now()));
            }
        })
    }

    async fn create_sync_run(&self, run: KnowledgeSyncRun) -> moa_knowledge::Result<()> {
        self.record_op("create_sync_run")?;
        self.with_state(|state| {
            state.sync_runs.insert(run.sync_run_uid, run);
        })
    }

    async fn claim_sync_run(&self, run: KnowledgeSyncRun) -> moa_knowledge::Result<SyncRunClaim> {
        self.record_op("claim_sync_run")?;
        self.with_state(|state| {
            if let Some(active) = state
                .sync_runs
                .values()
                .filter(|existing| existing.connection_uid == run.connection_uid)
                .filter(|existing| sync_run_is_active(existing.status))
                .max_by_key(|existing| (existing.started_at, existing.sync_run_uid))
                .cloned()
            {
                return SyncRunClaim::AlreadyRunning(active);
            }
            state.sync_runs.insert(run.sync_run_uid, run.clone());
            SyncRunClaim::Claimed(run)
        })
    }

    async fn get_sync_run(
        &self,
        sync_run_uid: Uuid,
    ) -> moa_knowledge::Result<Option<KnowledgeSyncRun>> {
        self.record_op("get_sync_run")?;
        self.with_state(|state| state.sync_runs.get(&sync_run_uid).cloned())
    }

    async fn latest_sync_run_for_connection(
        &self,
        connection_uid: Uuid,
        statuses: &[SyncRunStatus],
    ) -> moa_knowledge::Result<Option<KnowledgeSyncRun>> {
        self.record_op("latest_sync_run_for_connection")?;
        self.with_state(|state| {
            state
                .sync_runs
                .values()
                .filter(|run| run.connection_uid == connection_uid)
                .filter(|run| statuses.is_empty() || statuses.contains(&run.status))
                .max_by_key(|run| (run.started_at, run.sync_run_uid))
                .cloned()
        })
    }

    async fn update_sync_run(&self, mut run: KnowledgeSyncRun) -> moa_knowledge::Result<()> {
        self.record_op("update_sync_run")?;
        self.with_state(|state| {
            // The Postgres statement deliberately omits the trigger boundary, so
            // a status update can never erase evidence of a dispatch. Mirroring
            // that here is what makes the crash-replay tests meaningful.
            if let Some(existing) = state.sync_runs.get(&run.sync_run_uid) {
                run.provider_trigger_completed_at = existing.provider_trigger_completed_at;
            }
            state.sync_runs.insert(run.sync_run_uid, run);
        })
    }

    async fn add_sync_counters(
        &self,
        sync_run_uid: Uuid,
        counters: KnowledgeSyncCounters,
    ) -> moa_knowledge::Result<()> {
        self.record_op("add_sync_counters")?;
        self.with_state(|state| {
            if let Some(run) = state.sync_runs.get_mut(&sync_run_uid) {
                run.records_seen += counters.records_seen;
                run.records_changed += counters.records_changed;
                run.records_deleted += counters.records_deleted;
                run.records_ingested += counters.records_ingested;
                run.records_failed += counters.records_failed;
                run.objects_parsed += counters.objects_parsed;
                run.chunks_embedded += counters.chunks_embedded;
                run.graph_nodes_upserted += counters.graph_nodes_upserted;
                run.graph_edges_upserted += counters.graph_edges_upserted;
            }
        })
    }

    async fn record_ingestion_step(
        &self,
        step: KnowledgeIngestionStep,
    ) -> moa_knowledge::Result<()> {
        self.record_op("record_ingestion_step")?;
        self.with_state(|state| {
            state.steps.push(step);
        })
    }

    async fn record_ingestion_step_once(
        &self,
        step: KnowledgeIngestionStep,
        counter_delta: KnowledgeSyncCounters,
    ) -> moa_knowledge::Result<bool> {
        self.record_op("record_ingestion_step_once")?;
        self.with_state(|state| {
            let step_object = step.object_uid.unwrap_or(Uuid::nil());
            let exists = state.steps.iter().any(|existing| {
                existing.sync_run_uid == step.sync_run_uid
                    && existing.object_uid.unwrap_or(Uuid::nil()) == step_object
                    && existing.step == step.step
                    && existing.retry_count == step.retry_count
            });
            if exists {
                return false;
            }
            if let Some(run) = state.sync_runs.get_mut(&step.sync_run_uid) {
                run.records_seen += counter_delta.records_seen;
                run.records_changed += counter_delta.records_changed;
                run.records_deleted += counter_delta.records_deleted;
                run.records_ingested += counter_delta.records_ingested;
                run.records_failed += counter_delta.records_failed;
                run.objects_parsed += counter_delta.objects_parsed;
                run.chunks_embedded += counter_delta.chunks_embedded;
                run.graph_nodes_upserted += counter_delta.graph_nodes_upserted;
                run.graph_edges_upserted += counter_delta.graph_edges_upserted;
            }
            state.steps.push(step);
            true
        })
    }

    async fn sync_run_steps(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
    ) -> moa_knowledge::Result<Vec<KnowledgeIngestionStep>> {
        self.record_op("sync_run_steps")?;
        self.with_state(|state| {
            let mut steps = state
                .steps
                .iter()
                .filter(|step| step.sync_run_uid == sync_run_uid)
                .filter(|step| {
                    object_uid.is_none_or(|object_uid| step.object_uid == Some(object_uid))
                })
                .cloned()
                .collect::<Vec<_>>();
            steps.sort_by_key(|step| (step.started_at, step.step.clone(), step.retry_count));
            steps
        })
    }
}

#[async_trait]
impl KnowledgeAclRepository for InMemoryKnowledgeRepository {
    async fn replace_object_acl_snapshot(
        &self,
        snapshot: moa_knowledge::domain::ProviderAclSnapshot,
    ) -> moa_knowledge::Result<moa_knowledge::domain::ProviderAclSnapshot> {
        self.record_op("replace_object_acl_snapshot")?;
        // Mirrors the SQL writer exactly: an identical (revision, hash) capture
        // is idempotent, an incomplete capture is recorded but never becomes
        // current, and the object's pointer moves in the same operation.
        self.with_state(|state| {
            let existing = state.acl_snapshots.values().find(|stored| {
                stored.object_uid == snapshot.object_uid
                    && stored.provider_revision == snapshot.provider_revision
                    && stored.snapshot_hash == snapshot.snapshot_hash
            });
            let snapshot_uid = existing.map_or(snapshot.snapshot_uid, |stored| stored.snapshot_uid);
            let stored = moa_knowledge::domain::ProviderAclSnapshot {
                snapshot_uid,
                ..snapshot.clone()
            };
            state.acl_snapshots.insert(snapshot_uid, stored.clone());
            if let Some(object) = state.objects.get_mut(&snapshot.object_uid) {
                object.acl = if snapshot.complete {
                    moa_knowledge::domain::ObjectAcl::current(
                        snapshot.provider_revision.clone(),
                        snapshot_uid,
                    )
                } else {
                    moa_knowledge::domain::ObjectAcl::incomplete()
                };
            }
            stored
        })
    }

    async fn mark_object_acl_stale(
        &self,
        object_uid: Uuid,
        announced_revision: Option<&str>,
    ) -> moa_knowledge::Result<()> {
        self.record_op("mark_object_acl_stale")?;
        let announced = announced_revision.map(ToString::to_string);
        self.with_state(|state| {
            if let Some(object) = state.objects.get_mut(&object_uid) {
                object.acl.state = moa_knowledge::domain::SourceAclState::Stale;
                object.acl.revision = announced.or_else(|| object.acl.revision.clone());
                object.acl.current_snapshot_uid = None;
            }
        })
    }

    async fn object_acl(
        &self,
        object_uid: Uuid,
    ) -> moa_knowledge::Result<Option<moa_knowledge::domain::ObjectAcl>> {
        self.record_op("object_acl")?;
        self.with_state(|state| {
            state
                .objects
                .get(&object_uid)
                .map(|object| object.acl.clone())
        })
    }

    async fn snapshot_entries(
        &self,
        snapshot_uid: Uuid,
    ) -> moa_knowledge::Result<Vec<moa_knowledge::domain::ProviderAclEntry>> {
        self.record_op("snapshot_entries")?;
        self.with_state(|state| {
            state
                .acl_snapshots
                .get(&snapshot_uid)
                .map(|snapshot| snapshot.entries.clone())
                .unwrap_or_default()
        })
    }

    async fn upsert_principal_binding(
        &self,
        binding: moa_knowledge::domain::SourcePrincipalBinding,
    ) -> moa_knowledge::Result<()> {
        self.record_op("upsert_principal_binding")?;
        self.with_state(|state| {
            state.acl_bindings.retain(|stored| {
                !(stored.tenant_id == binding.tenant_id
                    && stored.contact_id == binding.contact_id
                    && stored.principal == binding.principal
                    && stored.connection_uid == binding.connection_uid)
            });
            state.acl_bindings.push(binding);
        })
    }

    async fn verified_principal_bindings(
        &self,
        connection_uid: Uuid,
        principals: &[moa_core::types::memory::SourcePrincipalFingerprint],
    ) -> moa_knowledge::Result<Vec<moa_knowledge::domain::SourcePrincipalBinding>> {
        self.record_op("verified_principal_bindings")?;
        self.with_state(|state| {
            state
                .acl_bindings
                .iter()
                .filter(|binding| {
                    binding.connection_uid == Some(connection_uid)
                        && principals.contains(&binding.principal)
                })
                .cloned()
                .collect()
        })
    }

    async fn upsert_group_binding(
        &self,
        binding: moa_knowledge::domain::SourcePrincipalGroupBinding,
    ) -> moa_knowledge::Result<()> {
        self.record_op("upsert_group_binding")?;
        self.with_state(|state| {
            state.acl_group_bindings.retain(|stored| {
                !(stored.tenant_id == binding.tenant_id
                    && stored.member == binding.member
                    && stored.group == binding.group
                    && stored.connection_uid == binding.connection_uid)
            });
            state.acl_group_bindings.push(binding);
        })
    }

    async fn revoke_contact_principals(&self, contact_id: Uuid) -> moa_knowledge::Result<u64> {
        self.record_op("revoke_contact_principals")?;
        self.with_state(|state| {
            let before = state.acl_bindings.len();
            state
                .acl_bindings
                .retain(|binding| binding.contact_id != contact_id);
            (before - state.acl_bindings.len()) as u64
        })
    }
}

#[async_trait]
impl KnowledgeIngestionRepository for InMemoryKnowledgeRepository {
    async fn upsert_object(&self, object: KnowledgeObject) -> moa_knowledge::Result<()> {
        self.record_op("upsert_object")?;
        self.with_state(|state| {
            state.objects.insert(object.object_uid, object);
        })
    }

    async fn get_object(&self, object_uid: Uuid) -> moa_knowledge::Result<Option<KnowledgeObject>> {
        self.record_op("get_object")?;
        self.with_state(|state| state.objects.get(&object_uid).cloned())
    }

    async fn list_objects(
        &self,
        tenant_id: TenantId,
        connection_uid: Option<Uuid>,
        object_type: Option<&str>,
        limit: u32,
    ) -> moa_knowledge::Result<Vec<KnowledgeObjectProjection>> {
        self.record_op("list_objects")?;
        self.with_state(|state| {
            state
                .objects
                .values()
                .filter(|object| object.tenant_id == tenant_id)
                .filter(|object| {
                    connection_uid
                        .is_none_or(|connection_uid| object.connection_uid == connection_uid)
                })
                .filter(|object| {
                    object_type.is_none_or(|object_type| object.object_type == object_type)
                })
                .take(limit as usize)
                .cloned()
                .map(|object| {
                    let version = state.versions.get(&object.object_uid);
                    let chunks = version
                        .and_then(|version| state.chunks.get(&version.version_uid))
                        .cloned()
                        .unwrap_or_default();
                    KnowledgeObjectProjection {
                        parser: version.map(|version| version.parser.clone()),
                        parser_status: if version.is_some() {
                            "parsed".to_string()
                        } else {
                            "pending".to_string()
                        },
                        chunk_count: chunks.len() as u64,
                        object,
                    }
                })
                .collect()
        })
    }

    async fn get_object_by_source(
        &self,
        connection_uid: Uuid,
        source_id: &str,
    ) -> moa_knowledge::Result<Option<KnowledgeObject>> {
        self.record_op("get_object_by_source")?;
        self.with_state(|state| {
            state
                .objects
                .values()
                .find(|object| {
                    object.connection_uid == connection_uid && object.source_id == source_id
                })
                .cloned()
        })
    }

    async fn unseen_active_objects_for_connection(
        &self,
        connection_uid: Uuid,
        tenant_id: TenantId,
        seen_source_ids: &[String],
        after: Option<(String, Uuid)>,
        limit: i64,
    ) -> moa_knowledge::Result<Vec<KnowledgeObject>> {
        self.record_op("unseen_active_objects_for_connection")?;
        self.with_state(|state| {
            let seen = seen_source_ids
                .iter()
                .map(String::as_str)
                .collect::<HashSet<_>>();
            let mut objects = state
                .objects
                .values()
                .filter(|object| object.connection_uid == connection_uid)
                .filter(|object| object.tenant_id == tenant_id)
                .filter(|object| object.status != ObjectStatus::Deleted)
                .filter(|object| !seen.contains(object.source_id.as_str()))
                .filter(|object| match &after {
                    Some((source_id, object_uid)) => {
                        (object.source_id.as_str(), object.object_uid)
                            > (source_id.as_str(), *object_uid)
                    }
                    None => true,
                })
                .cloned()
                .collect::<Vec<_>>();
            objects.sort_by(|left, right| {
                left.source_id
                    .cmp(&right.source_id)
                    .then_with(|| left.object_uid.cmp(&right.object_uid))
            });
            objects.truncate(usize::try_from(limit).unwrap_or(0));
            objects
        })
    }

    async fn latest_document_version(
        &self,
        object_uid: Uuid,
    ) -> moa_knowledge::Result<Option<DocumentVersion>> {
        self.record_op("latest_document_version")?;
        self.with_state(|state| state.versions.get(&object_uid).cloned())
    }

    async fn chunks_for_version(
        &self,
        version_uid: Uuid,
    ) -> moa_knowledge::Result<Vec<KnowledgeChunk>> {
        self.record_op("chunks_for_version")?;
        self.with_state(|state| state.chunks.get(&version_uid).cloned().unwrap_or_default())
    }

    async fn active_chunks_for_object(
        &self,
        object_uid: Uuid,
    ) -> moa_knowledge::Result<Vec<KnowledgeChunk>> {
        self.record_op("active_chunks_for_object")?;
        self.with_state(|state| {
            let Some(version) = state.versions.get(&object_uid) else {
                return Vec::new();
            };
            state
                .chunks
                .get(&version.version_uid)
                .map(|chunks| {
                    chunks
                        .iter()
                        .filter(|chunk| {
                            chunk.metadata.get("active").and_then(Value::as_bool) != Some(false)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    async fn object_ingestion_completed_since(
        &self,
        object_uid: Uuid,
        since: DateTime<Utc>,
    ) -> moa_knowledge::Result<bool> {
        self.record_op("object_ingestion_completed_since")?;
        self.with_state(|state| {
            state.steps.iter().any(|step| {
                step.object_uid == Some(object_uid)
                    && step.step == "contact_groups_derived"
                    && step.status == moa_knowledge::domain::IngestionStepStatus::Completed
                    && step
                        .counters
                        .get("records_ingested")
                        .and_then(Value::as_u64)
                        == Some(1)
                    && step.ended_at.unwrap_or(step.started_at) >= since
            })
        })
    }

    async fn inspect_object(
        &self,
        object_uid: Uuid,
    ) -> moa_knowledge::Result<Option<KnowledgeObjectInspection>> {
        self.record_op("inspect_object")?;
        self.with_state(|state| {
            let object = state.objects.get(&object_uid)?.clone();
            let version = state.versions.get(&object_uid).cloned();
            let chunks = version
                .as_ref()
                .and_then(|version| state.chunks.get(&version.version_uid))
                .cloned()
                .unwrap_or_default();
            let steps = state
                .steps
                .iter()
                .filter(|step| step.object_uid == Some(object_uid))
                .cloned()
                .collect();
            Some(KnowledgeObjectInspection {
                object,
                version,
                chunks,
                steps,
            })
        })
    }

    async fn insert_document_version(&self, version: DocumentVersion) -> moa_knowledge::Result<()> {
        self.record_op("insert_document_version")?;
        self.with_state(|state| {
            state.versions.insert(version.object_uid, version);
        })
    }

    async fn claim_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version: DocumentVersion,
    ) -> moa_knowledge::Result<DocumentVersionIngestionClaim> {
        self.record_op("claim_document_version_ingestion")?;
        self.with_state(|state| {
            let key = (version.object_uid, version.content_hash.clone());
            if let Some(existing) = state.ingestion_claims.get(&key) {
                match existing.status {
                    InMemoryDocumentIngestionClaimStatus::Started => {
                        return DocumentVersionIngestionClaim::AlreadyInProgress(
                            existing.version.clone(),
                        );
                    }
                    InMemoryDocumentIngestionClaimStatus::Completed => {
                        return DocumentVersionIngestionClaim::AlreadyCompleted(
                            existing.version.clone(),
                        );
                    }
                    InMemoryDocumentIngestionClaimStatus::Failed => {}
                }
            }

            let claim_token = Uuid::now_v7();
            state.versions.insert(version.object_uid, version.clone());
            state.ingestion_claims.insert(
                key,
                InMemoryDocumentIngestionClaim {
                    version: version.clone(),
                    sync_run_uid,
                    claim_token,
                    status: InMemoryDocumentIngestionClaimStatus::Started,
                },
            );
            DocumentVersionIngestionClaim::Claimed {
                version,
                claim_token,
            }
        })
    }

    async fn complete_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version_uid: Uuid,
        claim_token: Uuid,
    ) -> moa_knowledge::Result<()> {
        self.record_op("complete_document_version_ingestion")?;
        self.with_state(|state| {
            let Some(claim) = state
                .ingestion_claims
                .values_mut()
                .find(|claim| claim.version.version_uid == version_uid)
            else {
                return Err(KnowledgeError::Repository(
                    "document version ingestion claim not found".to_string(),
                ));
            };
            if claim.sync_run_uid != sync_run_uid
                || claim.claim_token != claim_token
                || claim.status != InMemoryDocumentIngestionClaimStatus::Started
            {
                return Err(KnowledgeError::Repository(
                    "document version ingestion claim token mismatch".to_string(),
                ));
            }
            claim.status = InMemoryDocumentIngestionClaimStatus::Completed;
            Ok(())
        })?
    }

    async fn fail_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version_uid: Uuid,
        claim_token: Uuid,
    ) -> moa_knowledge::Result<()> {
        self.record_op("fail_document_version_ingestion")?;
        self.with_state(|state| {
            let Some(claim) = state
                .ingestion_claims
                .values_mut()
                .find(|claim| claim.version.version_uid == version_uid)
            else {
                return Err(KnowledgeError::Repository(
                    "document version ingestion claim not found".to_string(),
                ));
            };
            if claim.sync_run_uid != sync_run_uid
                || claim.claim_token != claim_token
                || claim.status != InMemoryDocumentIngestionClaimStatus::Started
            {
                return Err(KnowledgeError::Repository(
                    "document version ingestion claim token mismatch".to_string(),
                ));
            }
            claim.status = InMemoryDocumentIngestionClaimStatus::Failed;
            Ok(())
        })?
    }

    async fn replace_blocks(
        &self,
        _version_uid: Uuid,
        _blocks: Vec<KnowledgeBlock>,
    ) -> moa_knowledge::Result<()> {
        self.record_op("replace_blocks")
    }

    async fn replace_chunks(
        &self,
        version_uid: Uuid,
        chunks: Vec<KnowledgeChunk>,
    ) -> moa_knowledge::Result<()> {
        self.record_op("replace_chunks")?;
        self.with_state(|state| {
            state.chunks.insert(version_uid, chunks);
        })
    }

    async fn tombstone_chunks(&self, _chunk_uids: &[Uuid]) -> moa_knowledge::Result<()> {
        self.record_op("tombstone_chunks")
    }

    async fn mark_object_deleted(
        &self,
        object_uid: Uuid,
        deleted_at: chrono::DateTime<chrono::Utc>,
    ) -> moa_knowledge::Result<()> {
        self.record_op("mark_object_deleted")?;
        self.with_state(|state| {
            if let Some(object) = state.objects.get_mut(&object_uid) {
                object.status = ObjectStatus::Deleted;
                object.deleted_at = Some(deleted_at);
            }
        })
    }
}

#[async_trait]
impl KnowledgeContactGroupRepository for InMemoryKnowledgeRepository {
    async fn upsert_contact_group(&self, _group: ContactGroup) -> moa_knowledge::Result<()> {
        self.record_op("upsert_contact_group")
    }

    async fn replace_contact_group_memberships(
        &self,
        _group_uid: Uuid,
        _memberships: Vec<ContactGroupMembership>,
    ) -> moa_knowledge::Result<()> {
        self.record_op("replace_contact_group_memberships")
    }

    async fn contact_group_targets(
        &self,
        _tenant_id: TenantId,
        _group_key: &str,
    ) -> moa_knowledge::Result<Option<ContactGroupTarget>> {
        self.record_op("contact_group_targets")?;
        Ok(None)
    }
}

#[async_trait]
impl KnowledgeEventRepository for InMemoryKnowledgeRepository {
    async fn record_provider_event(
        &self,
        event: KnowledgeProviderEventRecord,
    ) -> moa_knowledge::Result<KnowledgeProviderEventRecord> {
        self.record_op("record_provider_event")?;
        self.with_state(|state| {
            let key = (
                event.tenant_id,
                event.provider.clone(),
                event.provider_event_id.clone(),
            );
            if let Some(existing) = state.provider_events.get(&key) {
                let mut duplicate = existing.clone();
                duplicate.duplicate = true;
                return duplicate;
            }
            state.provider_events.insert(key, event.clone());
            event
        })
    }
}
