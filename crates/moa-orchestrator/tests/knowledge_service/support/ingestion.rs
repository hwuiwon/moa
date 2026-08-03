// Knowledge ingestion runner, parser, embedder, and record fixtures.

#[derive(Debug, Clone, Default)]
struct FakeKnowledgeIngestionRunner {
    calls: Arc<Mutex<Vec<FakeKnowledgeIngestionCall>>>,
}

impl FakeKnowledgeIngestionRunner {
    fn calls(&self) -> Vec<FakeKnowledgeIngestionCall> {
        self.calls
            .lock()
            .expect("fake ingestion runner calls should not be poisoned")
            .clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeKnowledgeIngestionCall {
    sync_run_uid: Uuid,
    connection_uid: Uuid,
    tenant_id: TenantId,
    provider: String,
    records_listed: u64,
}

#[async_trait]
impl KnowledgeIngestionRunner for FakeKnowledgeIngestionRunner {
    async fn ingest_record_page(
        &self,
        run: &KnowledgeSyncRun,
        provider: &str,
        page: RecordPage,
    ) -> Result<PageIngestionReport, KnowledgeServiceError> {
        let records_listed = page.records.len() as u64;
        self.calls
            .lock()
            .expect("fake ingestion runner calls should not be poisoned")
            .push(FakeKnowledgeIngestionCall {
                sync_run_uid: run.sync_run_uid,
                connection_uid: run.connection_uid,
                tenant_id: run.tenant_id,
                provider: provider.to_string(),
                records_listed,
            });
        Ok(PageIngestionReport {
            records_listed,
            records_ingested: records_listed,
            ..PageIngestionReport::default()
        })
    }

    async fn prune_unseen_objects(
        &self,
        run: &KnowledgeSyncRun,
        provider: &str,
        seen_source_ids: &HashSet<String>,
    ) -> Result<PageIngestionReport, KnowledgeServiceError> {
        self.calls
            .lock()
            .expect("fake ingestion runner calls should not be poisoned")
            .push(FakeKnowledgeIngestionCall {
                sync_run_uid: run.sync_run_uid,
                connection_uid: run.connection_uid,
                tenant_id: run.tenant_id,
                provider: provider.to_string(),
                records_listed: seen_source_ids.len() as u64,
            });
        Ok(PageIngestionReport::default())
    }
}

fn fixture_connection(tenant_id: TenantId) -> KnowledgeConnection {
    KnowledgeConnection {
        connection_uid: Uuid::now_v7(),
        tenant_id,
        provider: moa_knowledge::domain::LinkedProviderKind::Nango,
        connector: CONNECTOR.to_string(),
        provider_account_id: "provider-account-1".to_string(),
        metadata: json!({ "safe": "connection" }),
        source_selection: json!({}),
        information_barrier: None,
        created_at: moa_test_support::fixtures::pg_now(),
        updated_at: moa_test_support::fixtures::pg_now(),
        last_synced_at: None,
    }
}

fn fixture_connection_for_provider(
    tenant_id: TenantId,
    provider: &str,
    connector: &str,
    provider_account_id: &str,
) -> KnowledgeConnection {
    let mut connection = fixture_connection(tenant_id);
    connection.provider = linked_provider(provider);
    connection.connector = connector.to_string();
    connection.provider_account_id = provider_account_id.to_string();
    connection
}

fn fixture_object(tenant_id: TenantId, connection_uid: Uuid) -> KnowledgeObject {
    KnowledgeObject {
        acl: moa_knowledge::domain::ObjectAcl::incomplete(),
        object_uid: Uuid::now_v7(),
        tenant_id,
        connection_uid,
        object_type: "document".to_string(),
        source_id: "doc-1".to_string(),
        parent_source_id: None,
        source_uri: Some("https://example.test/doc-1".to_string()),
        title: Some("Rotation Runbook".to_string()),
        change_token: Some("etag-1".to_string()),
        metadata: json!({
            "safe": "object",
            "access_token": SECRET_TOKEN,
            "nested": { "authorization": SECRET_BEARER }
        }),
        status: ObjectStatus::Active,
        source_updated_at: Some(moa_test_support::fixtures::pg_now()),
        deleted_at: None,
    }
}

fn fixture_version(object_uid: Uuid) -> DocumentVersion {
    DocumentVersion {
        version_uid: Uuid::now_v7(),
        object_uid,
        parser: "native".to_string(),
        parser_job_id: Some("job-1".to_string()),
        content_hash: "content-hash".to_string(),
        metadata: json!({
            "safe": "version",
            "refresh_token": SECRET_TOKEN
        }),
        created_at: moa_test_support::fixtures::pg_now(),
    }
}

async fn complete_sync_run(
    repository: &PostgresKnowledgeRepository,
    sync_run_uid: Uuid,
) -> moa_knowledge::Result<()> {
    let Some(mut run) = repository.get_sync_run(sync_run_uid).await? else {
        return Err(KnowledgeError::Repository(format!(
            "missing sync run {sync_run_uid}"
        )));
    };
    run.status = SyncRunStatus::Completed;
    run.finished_at = Some(moa_test_support::fixtures::pg_now());
    repository.update_sync_run(run).await
}

async fn seed_task14_embedder_state(pool: &sqlx::PgPool, tenant_id: TenantId) {
    let mut conn = ScopedConn::begin_tenant(pool, tenant_id)
        .await
        .expect("begin Task14 embedder state seed transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role for Task14 embedder state seed");
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension
        "#,
    )
    .bind(StoragePartitionId::for_tenant(tenant_id).to_string())
    .bind(TASK14_EMBEDDING_MODEL)
    .bind(TASK14_EMBEDDING_MODEL_VERSION)
    .bind(VECTOR_DIMENSION as i32)
    .execute(conn.as_mut())
    .await
    .expect("seed Task14 storage partition embedder state");
    conn.commit()
        .await
        .expect("commit Task14 embedder state seed");
}

async fn insert_retrieval_lineage_row(
    pool: &sqlx::PgPool,
    event: LineageEvent,
    trace_uid: Uuid,
    tenant_id: TenantId,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO analytics.turn_lineage (
            turn_id,
            session_id,
            user_id,
            storage_partition_id,
            ts,
            tier,
            record_kind,
            payload,
            integrity_hash,
            prev_hash
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NULL)
        "#,
    )
    .bind(trace_uid)
    .bind(SessionId::new().0)
    .bind("task14-contact")
    .bind(StoragePartitionId::for_tenant(tenant_id).to_string())
    .bind(moa_test_support::fixtures::pg_now())
    .bind(1_i16)
    .bind(RecordKind::Retrieval.as_i16())
    .bind(serde_json::to_value(event).expect("retrieval lineage should serialize"))
    .bind(vec![0_u8; 32])
    .execute(pool)
    .await
    .map(|_| ())
}

fn assert_sync_status_counters(
    status: &moa_wire::knowledge::KnowledgeSyncStatusResponse,
    expected_records: u64,
    expected_graph_nodes: u64,
    expected_graph_edges: u64,
) {
    assert_eq!(status.status, "completed");
    assert_eq!(status.records_seen, expected_records);
    assert_eq!(status.records_changed, expected_records);
    assert_eq!(status.records_deleted, 0);
    assert_eq!(status.records_ingested, expected_records);
    assert_eq!(status.records_failed, 0);
    assert_eq!(status.objects_parsed, expected_records);
    assert_eq!(status.chunks_embedded, expected_records);
    // Print both graph counters so a topology change is visible in one failure.
    assert_eq!(
        status.graph_nodes_upserted, expected_graph_nodes,
        "graph nodes upserted changed: observed {} nodes / {} edges",
        status.graph_nodes_upserted, status.graph_edges_upserted
    );
    assert_eq!(
        status.graph_edges_upserted, expected_graph_edges,
        "graph edges upserted changed: observed {} nodes / {} edges",
        status.graph_nodes_upserted, status.graph_edges_upserted
    );
}

fn object_ingestion_steps() -> Vec<&'static str> {
    vec![
        // The ACL is captured ahead of both content fences, so it is the first
        // per-record step of every ingestion — including one whose content
        // turns out to be unchanged.
        "source_acl_captured",
        "object_change_checked",
        "content_fetched",
        "parse_submitted",
        "parse_completed",
        "normalized",
        "blocks_diffed",
        "chunks_diffed",
        "embedded",
        "graph_upserted",
        "vector_indexed",
        "contact_groups_derived",
    ]
}

async fn create_contact_group_graph_node(
    graph: &PostgresGraphStore,
    tenant_id: TenantId,
    group: &ContactGroup,
) -> moa_memory_graph::Result<Uuid> {
    graph
        .create_node(NodeWriteIntent {
            barrier: None,
            uid: group.group_uid,
            data_subject_id: tenant_id.0,
            label: NodeLabel::ContactGroup,
            storage_partition_id: Some(tenant_id.to_string()),
            contact_id: None,
            scope: "tenant".to_string(),
            name: group.display_name.clone(),
            properties: json!({
                "group_key": group.group_key,
                "display_name": group.display_name,
            }),
            pii_class: SensitivityClass::None,
            confidence: Some(0.95),
            valid_from: moa_test_support::fixtures::pg_now(),
            embedding: None,
            embedding_model: None,
            embedding_model_version: None,
            embedding_text: None,
            actor_id: Uuid::now_v7().to_string(),
            actor_kind: "system".to_string(),
        })
        .await
}

async fn graph_label_counts(pool: &sqlx::PgPool, tenant_id: TenantId) -> HashMap<String, i64> {
    sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT label::TEXT, count(*)
        FROM moa.node_index
        WHERE storage_partition_id = $1
          AND valid_to IS NULL
        GROUP BY label
        "#,
    )
    .bind(tenant_id.to_string())
    .fetch_all(pool)
    .await
    .expect("read graph label counts")
    .into_iter()
    .collect()
}

async fn chunk_vector_row_count(pool: &sqlx::PgPool, tenant_id: TenantId) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.embeddings
        WHERE storage_partition_id = $1
          AND label = 'Chunk'
        "#,
    )
    .bind(tenant_id.to_string())
    .fetch_one(pool)
    .await
    .expect("read chunk vector row count")
}

#[derive(Debug, Default)]
struct Task14Parser;

#[async_trait]
impl DocumentParser for Task14Parser {
    async fn parse(&self, input: ParseInput) -> moa_knowledge::Result<ParsedDocument> {
        match input.object.source_id.as_str() {
            "merge-md-handbook" => Ok(parsed_doc(
                "native",
                None,
                "Benefits Handbook",
                json!({ "job_status": "completed", "format": "markdown" }),
                vec![
                    element(
                        "md-heading-1",
                        DocumentElementKind::Heading,
                        "PTO Policy",
                        vec!["Benefits Handbook", "PTO Policy"],
                        0,
                        None,
                        json!({ "markdown_heading_level": 1 }),
                    ),
                    element(
                        "md-paragraph-1",
                        DocumentElementKind::Paragraph,
                        "PTO policy is standardized for all employees.",
                        vec!["Benefits Handbook", "PTO Policy"],
                        1,
                        None,
                        json!({ "markdown": true }),
                    ),
                    element(
                        "md-list-1",
                        DocumentElementKind::ListItem,
                        "Carryover is capped at five days.",
                        vec!["Benefits Handbook", "PTO Policy"],
                        2,
                        None,
                        json!({ "list_marker": "-" }),
                    ),
                ],
            )),
            "nango-llamaparse-policy" => Ok(parsed_doc(
                "llamaparse",
                Some("lp-task14-job"),
                "Finance Controls",
                json!({
                    "job_status": "completed",
                    "markdown": true,
                    "items": 2,
                    "job_metadata": { "pages": 1 }
                }),
                vec![
                    element(
                        "lp-heading-1",
                        DocumentElementKind::Heading,
                        "Finance Controls",
                        vec!["Finance Controls"],
                        0,
                        None,
                        json!({ "llamaparse_item_type": "heading" }),
                    ),
                    element(
                        "lp-item-1",
                        DocumentElementKind::ListItem,
                        "Finance control is dual approval before payroll export.",
                        vec!["Finance Controls"],
                        1,
                        None,
                        json!({ "llamaparse_item_id": "item-1" }),
                    ),
                ],
            )),
            "nango-unstructured-guide" => Ok(parsed_doc(
                "unstructured",
                Some("unstructured-task14-job"),
                "Support Guide",
                json!({ "job_status": "completed", "element_count": 2 }),
                vec![
                    element(
                        "un-title-1",
                        DocumentElementKind::Heading,
                        "Support Guide",
                        vec!["Support Guide"],
                        0,
                        None,
                        json!({ "unstructured_type": "Title" }),
                    ),
                    element(
                        "un-narrative-1",
                        DocumentElementKind::Paragraph,
                        "Support guide is escalated when billing evidence is missing.",
                        vec!["Support Guide"],
                        1,
                        Some(ElementLayout {
                            x: 12.0,
                            y: 24.0,
                            width: 300.0,
                            height: 90.0,
                            page_width: Some(612.0),
                            page_height: Some(792.0),
                            confidence: Some(0.99),
                        }),
                        json!({ "filename": "support-guide.pdf" }),
                    ),
                ],
            )),
            "nango-reducto-layout" => Ok(parsed_doc(
                "reducto",
                Some("reducto-task14-job"),
                "Warehouse Layout",
                json!({
                    "job_status": "completed",
                    "usage": { "pages": 1 },
                    "studio_link": "https://reducto.example.test/studio/task14",
                    "blocks": [
                        {
                            "type": "paragraph",
                            "bbox": [0.1, 0.2, 0.7, 0.4]
                        }
                    ]
                }),
                vec![element(
                    "reducto-chunk-1",
                    DocumentElementKind::ParserChunk,
                    "Warehouse layout is receiving on the east dock.",
                    vec!["Warehouse Layout"],
                    0,
                    Some(ElementLayout {
                        x: 0.1,
                        y: 0.2,
                        width: 0.6,
                        height: 0.2,
                        page_width: Some(1.0),
                        page_height: Some(1.0),
                        confidence: Some(0.98),
                    }),
                    json!({
                        "blocks": [
                            {
                                "type": "paragraph",
                                "bbox": [0.1, 0.2, 0.7, 0.4]
                            }
                        ]
                    }),
                )],
            )),
            "merge-crm-contact" => Ok(parsed_doc(
                "native",
                None,
                "CRM Contact",
                json!({ "job_status": "completed", "format": "crm_contact" }),
                vec![element(
                    "crm-contact-field-1",
                    DocumentElementKind::Field,
                    "CRM contact is linked to the existing MOA contact.",
                    vec!["CRM Contact"],
                    0,
                    None,
                    json!({ "crm_model": "contact", "moa_contact_linked": true }),
                )],
            )),
            "merge-crm-account" => Ok(parsed_doc(
                "native",
                None,
                "Acme Account",
                json!({ "job_status": "completed", "format": "crm_account" }),
                vec![element(
                    "crm-account-field-1",
                    DocumentElementKind::Field,
                    "Acme account is the enterprise renewal group.",
                    vec!["Acme Account"],
                    0,
                    None,
                    json!({ "crm_model": "account" }),
                )],
            )),
            source_id => Err(KnowledgeError::parser(
                "task14",
                format!("unexpected task14 source id {source_id}"),
            )),
        }
    }
}

fn parsed_doc(
    parser: &str,
    parser_job_id: Option<&str>,
    fallback_title: &str,
    metadata: Value,
    elements: Vec<DocumentElement>,
) -> ParsedDocument {
    let text = elements
        .iter()
        .map(|element| element.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    ParsedDocument {
        parser: parser.to_string(),
        parser_job_id: parser_job_id.map(ToOwned::to_owned),
        text: if text.is_empty() {
            fallback_title.to_string()
        } else {
            text
        },
        elements,
        metadata,
    }
}

fn element(
    element_id: &str,
    kind: DocumentElementKind,
    text: &str,
    heading_path: Vec<&str>,
    ordinal: u32,
    layout: Option<ElementLayout>,
    metadata: Value,
) -> DocumentElement {
    DocumentElement {
        element_id: element_id.to_string(),
        kind,
        text: text.to_string(),
        heading_path: heading_path.into_iter().map(ToOwned::to_owned).collect(),
        ordinal,
        page_number: Some(1),
        layout,
        metadata,
    }
}

#[derive(Debug, Default)]
struct Task14Embedder;

const TASK14_EMBEDDING_MODEL: &str = "embed-v4.0";
const TASK14_EMBEDDING_MODEL_VERSION: i32 = 1;

#[async_trait]
impl EmbeddingProvider for Task14Embedder {
    fn model_id(&self) -> &str {
        TASK14_EMBEDDING_MODEL
    }

    fn dimensions(&self) -> usize {
        VECTOR_DIMENSION
    }

    fn model_version(&self) -> i32 {
        TASK14_EMBEDDING_MODEL_VERSION
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
        Ok(inputs.iter().map(|input| task14_vector(input)).collect())
    }
}

fn task14_vector(input: &str) -> Vec<f32> {
    let mut vector = vec![0.0; VECTOR_DIMENSION];
    for (index, byte) in input.bytes().enumerate() {
        vector[index % VECTOR_DIMENSION] += f32::from(byte) / 255.0;
    }
    vector[0] += 1.0;
    vector
}

fn task14_merge_records() -> Vec<ProviderRecord> {
    vec![
        provider_record(
            "merge-md-handbook",
            "article",
            "Benefits Handbook",
            "https://merge.example.test/kb/benefits",
            "# PTO Policy\n\nPTO policy is standardized for all employees.\n\n- Carryover is capped at five days.",
            json!({ "mime_type": "text/markdown", "merge": { "category": "knowledge" } }),
        ),
        provider_record(
            "merge-crm-contact",
            "crm_contact",
            "CRM Contact",
            "https://merge.example.test/crm/contact/member-a",
            "CRM contact is linked to the existing MOA contact.",
            json!({
                "mime_type": "application/json",
                "merge": {
                    "contact": { "id": "contact-task14", "name": "Member A" },
                    "account": { "id": "acct-task14", "name": "Acme" }
                }
            }),
        ),
        provider_record(
            "merge-crm-account",
            "crm_account",
            "Acme Account",
            "https://merge.example.test/crm/account/acct-task14",
            "Acme account is the enterprise renewal group.",
            json!({
                "mime_type": "application/json",
                "merge": {
                    "account": { "id": "acct-task14", "name": "Acme" },
                    "members": [
                        { "email": "member-a@example.invalid" }
                    ]
                }
            }),
        ),
    ]
}

fn task14_nango_records() -> Vec<ProviderRecord> {
    vec![
        provider_record(
            "nango-llamaparse-policy",
            "document",
            "Finance Controls",
            "https://nango.example.test/docs/finance-controls",
            "Finance control is dual approval before payroll export.",
            json!({ "mime_type": "application/pdf", "parser": "llamaparse" }),
        ),
        provider_record(
            "nango-unstructured-guide",
            "document",
            "Support Guide",
            "https://nango.example.test/docs/support-guide",
            "Support guide is escalated when billing evidence is missing.",
            json!({ "mime_type": "application/pdf", "parser": "unstructured" }),
        ),
        provider_record(
            "nango-reducto-layout",
            "document",
            "Warehouse Layout",
            "https://nango.example.test/docs/warehouse-layout",
            "Warehouse layout is receiving on the east dock.",
            json!({ "mime_type": "application/pdf", "parser": "reducto" }),
        ),
    ]
}

fn provider_record(
    source_id: &str,
    object_type: &str,
    title: &str,
    source_uri: &str,
    text: &str,
    metadata: Value,
) -> ProviderRecord {
    ProviderRecord {
        acl: provider_record_acl(),
        materialization: moa_knowledge::domain::ProviderRecordMaterialization::InlineText {
            text: text.to_string(),
            mime_type: Some("text/plain".to_string()),
        },
        source_id: source_id.to_string(),
        object_type: object_type.to_string(),
        title: Some(title.to_string()),
        source_uri: Some(source_uri.to_string()),
        change_token: Some(format!("{source_id}-v1")),
        deleted: false,
        source_updated_at: Some(moa_test_support::fixtures::pg_now()),
        metadata,
        payload: json!({ "text": text }),
    }
}
