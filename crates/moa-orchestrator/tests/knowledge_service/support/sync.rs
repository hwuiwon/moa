// Knowledge sync workflow fakes and prepared-run fixtures.

#[derive(Debug)]
struct FakeKnowledgeSyncIngestionSteps {
    prepared: KnowledgeSyncPreparedRun,
    pages: Vec<RecordPage>,
    list_calls: Vec<FakeListPageCall>,
    apply_calls: Vec<FakeApplyPageCall>,
    prune_calls: Vec<FakePruneCall>,
    fail_calls: Vec<FakeFailCall>,
    status_transitions: Vec<SyncRunStatus>,
}

impl FakeKnowledgeSyncIngestionSteps {
    fn new(prepared: KnowledgeSyncPreparedRun) -> Self {
        Self {
            prepared,
            pages: Vec::new(),
            list_calls: Vec::new(),
            apply_calls: Vec::new(),
            prune_calls: Vec::new(),
            fail_calls: Vec::new(),
            status_transitions: Vec::new(),
        }
    }

    fn with_pages(mut self, pages: Vec<RecordPage>) -> Self {
        self.pages = pages;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeListPageCall {
    cursor: Option<String>,
    seen_cursors: Vec<String>,
    limit: u32,
    page_index: u32,
    modified_after: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeListChangedRecordsRequest {
    connection_uid: Uuid,
    cursor: Option<String>,
    limit: Option<u32>,
    modified_after: Option<DateTime<Utc>>,
    variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeApplyPageCall {
    page_index: u32,
    source_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakePruneCall {
    source_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeFailCall {
    stage: &'static str,
    error_message: String,
}

#[async_trait]
impl KnowledgeSyncIngestionSteps for FakeKnowledgeSyncIngestionSteps {
    async fn prepare_ingestion_run(
        &mut self,
        request: &KnowledgeSyncIngestionRequest,
    ) -> Result<KnowledgeSyncPreparedRun, HandlerError> {
        if request.sync_run_uid != self.prepared.run.sync_run_uid {
            return Err(TerminalError::new_with_code(404, "sync run mismatch").into());
        }
        self.status_transitions.push(self.prepared.run.status);
        self.prepared.run.status = SyncRunStatus::Ingesting;
        self.status_transitions.push(self.prepared.run.status);
        Ok(self.prepared.clone())
    }

    async fn list_changed_records_page(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        cursor: Option<String>,
        limit: u32,
        page_index: u32,
        seen_cursors: Vec<String>,
    ) -> Result<KnowledgeSyncProviderPage, HandlerError> {
        self.list_calls.push(FakeListPageCall {
            cursor,
            seen_cursors,
            limit,
            page_index,
            modified_after: prepared.connection.last_synced_at,
        });
        let page = if self.pages.is_empty() {
            RecordPage {
                records: Vec::new(),
                next_cursor: None,
            }
        } else {
            self.pages.remove(0)
        };
        let records_listed = page.records.len() as u64;
        Ok(KnowledgeSyncProviderPage {
            provider: prepared.provider,
            page,
            records_listed,
        })
    }

    async fn apply_record_page(
        &mut self,
        _prepared: &KnowledgeSyncPreparedRun,
        page: KnowledgeSyncProviderPage,
        page_index: u32,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError> {
        let source_ids = page
            .page
            .records
            .iter()
            .map(|record| record.source_id.clone())
            .collect::<Vec<_>>();
        let records_applied = source_ids.len() as u64;
        self.apply_calls.push(FakeApplyPageCall {
            page_index,
            source_ids,
        });
        Ok(KnowledgeSyncPageApplication {
            records_listed: page.records_listed,
            records_ingested: records_applied,
            records_skipped: 0,
            records_deleted: 0,
            embeddings_created: 0,
            records_applied,
        })
    }

    async fn prune_unseen_objects(
        &mut self,
        _prepared: &KnowledgeSyncPreparedRun,
        seen_source_ids: HashSet<String>,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError> {
        let mut source_ids = seen_source_ids.into_iter().collect::<Vec<_>>();
        source_ids.sort();
        self.prune_calls.push(FakePruneCall { source_ids });
        Ok(KnowledgeSyncPageApplication {
            records_listed: 0,
            records_ingested: 0,
            records_skipped: 0,
            records_deleted: 0,
            embeddings_created: 0,
            records_applied: 0,
        })
    }

    async fn complete_ingestion_run(
        &mut self,
        _prepared: &KnowledgeSyncPreparedRun,
    ) -> Result<(), HandlerError> {
        self.status_transitions.push(SyncRunStatus::Completed);
        Ok(())
    }

    async fn fail_ingestion_run(
        &mut self,
        _prepared: &KnowledgeSyncPreparedRun,
        stage: &'static str,
        error_message: String,
    ) -> Result<(), HandlerError> {
        self.fail_calls.push(FakeFailCall {
            stage,
            error_message,
        });
        Ok(())
    }
}

fn fake_prepared_sync_run(
    tenant_id: TenantId,
    connection_uid: Uuid,
    sync_run_uid: Uuid,
    max_records: u32,
) -> KnowledgeSyncPreparedRun {
    KnowledgeSyncPreparedRun {
        run: KnowledgeSyncRun {
            sync_run_uid,
            tenant_id,
            connection_uid,
            parser: Some("native".to_string()),
            max_records: Some(max_records),
            information_barrier: None,
            status: SyncRunStatus::ProviderSynced,
            records_seen: 0,
            records_changed: 0,
            records_deleted: 0,
            records_ingested: 0,
            records_failed: 0,
            objects_parsed: 0,
            chunks_embedded: 0,
            graph_nodes_upserted: 0,
            graph_edges_upserted: 0,
            error_code: None,
            started_at: moa_test_support::fixtures::pg_now(),
            finished_at: None,
            provider_trigger_completed_at: None,
        },
        connection: KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: moa_knowledge::domain::LinkedProviderKind::Merge,
            connector: CONNECTOR.to_string(),
            provider_account_id: "provider-account-1".to_string(),
            metadata: json!({}),
            source_selection: json!({}),
            information_barrier: None,
            created_at: moa_test_support::fixtures::pg_now(),
            updated_at: moa_test_support::fixtures::pg_now(),
            last_synced_at: None,
        },
        provider: moa_knowledge::domain::LinkedProviderKind::Merge,
        parser_label: "native".to_string(),
        page_size: 100,
        max_records,
    }
}

fn fake_record_page(source_ids: &[&str], next_cursor: Option<&str>) -> RecordPage {
    RecordPage {
        records: source_ids
            .iter()
            .map(|source_id| ProviderRecord {
                acl: provider_record_acl(),
                materialization:
                    moa_knowledge::domain::ProviderRecordMaterialization::InlineText {
                        text: (*source_id).to_string(),
                        mime_type: Some("text/plain".to_string()),
                    },
                source_id: (*source_id).to_string(),
                object_type: "document".to_string(),
                title: Some((*source_id).to_string()),
                source_uri: None,
                change_token: Some(format!("{source_id}-etag")),
                deleted: false,
                source_updated_at: None,
                metadata: json!({}),
                payload: json!({ "text": source_id }),
            })
            .collect(),
        next_cursor: next_cursor.map(ToOwned::to_owned),
    }
}

struct DbKnowledgeAutoSyncSteps {
    repository: Arc<PostgresKnowledgeRepository>,
    provider: Arc<Task14LinkedIntegrationProvider>,
    pipeline: Arc<KnowledgeIngestionPipeline>,
    page_size: u32,
    parser_label: String,
}

impl DbKnowledgeAutoSyncSteps {
    fn new(
        repository: Arc<PostgresKnowledgeRepository>,
        provider: Arc<Task14LinkedIntegrationProvider>,
        pipeline: Arc<KnowledgeIngestionPipeline>,
        page_size: u32,
        parser_label: impl Into<String>,
    ) -> Self {
        Self {
            repository,
            provider,
            pipeline,
            page_size,
            parser_label: parser_label.into(),
        }
    }
}

#[async_trait]
impl KnowledgeSyncIngestionSteps for DbKnowledgeAutoSyncSteps {
    async fn prepare_ingestion_run(
        &mut self,
        request: &KnowledgeSyncIngestionRequest,
    ) -> Result<KnowledgeSyncPreparedRun, HandlerError> {
        let mut run = self
            .repository
            .get_sync_run(request.sync_run_uid)
            .await
            .map_err(test_handler_error)?
            .ok_or_else(|| TerminalError::new_with_code(404, "knowledge sync run not found"))?;
        let connection = self
            .repository
            .get_connection(run.connection_uid)
            .await
            .map_err(test_handler_error)?
            .ok_or_else(|| TerminalError::new_with_code(404, "knowledge connection not found"))?;
        if connection.tenant_id != run.tenant_id || connection.connection_uid != run.connection_uid
        {
            return Err(
                TerminalError::new_with_code(404, "knowledge connection tenant mismatch").into(),
            );
        }
        let max_records = run.max_records.unwrap_or(100);
        run.status = SyncRunStatus::Ingesting;
        run.parser = Some(self.parser_label.clone());
        self.repository
            .update_sync_run(run.clone())
            .await
            .map_err(test_handler_error)?;
        Ok(KnowledgeSyncPreparedRun {
            provider: connection.provider,
            run,
            connection,
            parser_label: self.parser_label.clone(),
            page_size: self.page_size,
            max_records,
        })
    }

    async fn list_changed_records_page(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        cursor: Option<String>,
        limit: u32,
        _page_index: u32,
        _seen_cursors: Vec<String>,
    ) -> Result<KnowledgeSyncProviderPage, HandlerError> {
        let page = self
            .provider
            .list_changed_records(ListChangedRecordsRequest {
                acl_key: std::sync::Arc::new(moa_knowledge::acl_key::SourceAclKey::new(
                    1,
                    vec![7; 32],
                )),
                connection: prepared.connection.clone(),
                // The workflow body under test owns paging, not resolution; the
                // durable steps resolve through the shared owner instead.
                credential: Some(RedactedSecret::new("resolved-provider-token".to_string())),
                cursor,
                modified_after: prepared.connection.last_synced_at,
                limit: Some(limit),
                variant: None,
            })
            .await
            .map_err(test_handler_error)?;
        let records_listed = page.records.len() as u64;
        Ok(KnowledgeSyncProviderPage {
            provider: prepared.provider,
            page,
            records_listed,
        })
    }

    async fn apply_record_page(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        page: KnowledgeSyncProviderPage,
        _page_index: u32,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError> {
        let report = self
            .pipeline
            .ingest_record_page(
                prepared.run.sync_run_uid,
                prepared.run.connection_uid,
                prepared.run.tenant_id,
                page.page,
            )
            .await
            .map_err(test_handler_error)?;
        Ok(KnowledgeSyncPageApplication::from(report))
    }

    async fn prune_unseen_objects(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        seen_source_ids: HashSet<String>,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError> {
        let report = self
            .pipeline
            .prune_unseen_objects(
                prepared.run.sync_run_uid,
                prepared.run.connection_uid,
                prepared.run.tenant_id,
                &seen_source_ids,
            )
            .await
            .map_err(test_handler_error)?;
        Ok(KnowledgeSyncPageApplication::from(report))
    }

    async fn complete_ingestion_run(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
    ) -> Result<(), HandlerError> {
        let mut run = self
            .repository
            .get_sync_run(prepared.run.sync_run_uid)
            .await
            .map_err(test_handler_error)?
            .ok_or_else(|| TerminalError::new_with_code(404, "knowledge sync run not found"))?;
        run.status = SyncRunStatus::Completed;
        run.error_code = None;
        run.finished_at = Some(moa_test_support::fixtures::pg_now());
        self.repository
            .update_sync_run(run)
            .await
            .map_err(test_handler_error)?;

        let mut connection = self
            .repository
            .get_connection(prepared.run.connection_uid)
            .await
            .map_err(test_handler_error)?
            .ok_or_else(|| TerminalError::new_with_code(404, "knowledge connection not found"))?;
        connection.last_synced_at = Some(moa_test_support::fixtures::pg_now());
        self.repository
            .upsert_connection(connection)
            .await
            .map_err(test_handler_error)?;
        Ok(())
    }

    async fn fail_ingestion_run(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        stage: &'static str,
        error_message: String,
    ) -> Result<(), HandlerError> {
        let Some(mut run) = self
            .repository
            .get_sync_run(prepared.run.sync_run_uid)
            .await
            .map_err(test_handler_error)?
        else {
            return Ok(());
        };
        let classification = moa_knowledge::observability::classify_failure(
            stage,
            &KnowledgeError::provider(prepared.provider.as_str(), error_message),
        );
        run.status = if classification.retryable {
            SyncRunStatus::FailedRetryable
        } else {
            SyncRunStatus::FailedTerminal
        };
        run.records_failed = run.records_failed.saturating_add(1);
        run.error_code = Some(classification.error_code.to_string());
        run.finished_at = Some(moa_test_support::fixtures::pg_now());
        self.repository
            .update_sync_run(run)
            .await
            .map_err(test_handler_error)?;
        Ok(())
    }
}

async fn create_provider_synced_run(
    repository: &PostgresKnowledgeRepository,
    tenant_id: TenantId,
    connection_uid: Uuid,
    max_records: Option<u32>,
) -> Uuid {
    let sync_run_uid = Uuid::now_v7();
    repository
        .create_sync_run(KnowledgeSyncRun {
            sync_run_uid,
            tenant_id,
            connection_uid,
            parser: Some("task14".to_string()),
            max_records,
            information_barrier: None,
            status: SyncRunStatus::ProviderSynced,
            records_seen: 0,
            records_changed: 0,
            records_deleted: 0,
            records_ingested: 0,
            records_failed: 0,
            objects_parsed: 0,
            chunks_embedded: 0,
            graph_nodes_upserted: 0,
            graph_edges_upserted: 0,
            error_code: None,
            started_at: moa_test_support::fixtures::pg_now(),
            finished_at: None,
            provider_trigger_completed_at: None,
        })
        .await
        .expect("create provider-synced sync run");
    sync_run_uid
}

fn task14_ingestion_pipeline(
    pool: sqlx::PgPool,
    repository: Arc<PostgresKnowledgeRepository>,
    tenant_id: TenantId,
    provider: &str,
) -> Arc<KnowledgeIngestionPipeline> {
    let scope = RlsContext::tenant(tenant_id);
    let vector = Arc::new(PgvectorStore::new_for_app_role(pool.clone(), scope.clone()));
    let graph_store = Arc::new(
        PostgresGraphStore::scoped_for_app_role(
            pool,
            scope.clone(),
            Arc::new(moa_crypto::LocalKmsProvider::new()),
        )
        .with_vector_store(vector),
    );
    let graph_writer = Arc::new(MemoryKnowledgeGraphWriter::new(
        graph_store,
        MemoryScope::Tenant { tenant_id },
        "knowledge-auto-sync-test",
        None,
    ));
    Arc::new(KnowledgeIngestionPipeline::new(
        repository,
        Arc::new(Task14Parser),
        Arc::new(Task14Embedder),
        graph_writer,
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 128,
                max_tokens: 256,
                min_tokens: 1,
            },
            provider: provider.to_string(),
            parser_label: "task14".to_string(),
        },
    ))
}

fn test_handler_error(error: impl std::fmt::Display) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}

fn handler_error_text(error: &HandlerError) -> String {
    let error_ref = <HandlerError as AsRef<dyn std::error::Error + Send + Sync>>::as_ref(error);
    error_ref.to_string()
}
