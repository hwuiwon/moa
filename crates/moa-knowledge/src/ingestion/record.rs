//! Knowledge ingestion record operations.

use super::steps::record_span_outcome;
use super::*;

impl KnowledgeIngestionPipeline {
    /// Parses `input`, routing text-only records to the native parser.
    ///
    /// When the input carries neither bytes nor a `source_url`, an external
    /// document parser has nothing to fetch or upload, so the native parser is
    /// used even if an external parser is configured. Inputs with bytes or a
    /// source URL always go to the configured parser. See
    /// [`crate::parser::is_external_document_parser`].
    pub(super) async fn parse_document(
        &self,
        input: ParseInput,
        parse_span: &Span,
    ) -> Result<ParsedDocument> {
        if use_native_document_fallback(&input, &self.parser_label) {
            tracing::debug!(
                configured_parser = %self.parser_label,
                "record has only inline text; using the native parser instead of the configured external parser"
            );
            crate::parser::native::NativeDocumentParser::new()
                .parse(input)
                .instrument(parse_span.clone())
                .await
        } else {
            self.parser
                .parse(input)
                .instrument(parse_span.clone())
                .await
        }
    }

    pub(super) async fn ingest_record(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
        record: ProviderRecord,
    ) -> Result<RecordIngestionOutcome> {
        let existing = self
            .ingestion_repository
            .get_object_by_source(object.connection_uid, &object.source_id)
            .await?;

        // The object row must exist before its ACL snapshot can reference it. A
        // brand-new object lands `incomplete` — invisible — and only the capture
        // below can make it readable.
        if existing.is_none() {
            self.ingestion_repository
                .upsert_object(object.clone())
                .await?;
        }
        // Ahead of BOTH content fences: an unshared folder must stop being
        // retrievable on the next sync pass even though not one byte changed,
        // and re-parsing a document to learn that is pure waste.
        self.capture_record_acl(sync_run_uid, &object, &record)
            .await?;

        if record.materialization.is_metadata_only() {
            let changed = existing.as_ref().is_none_or(|existing| {
                existing.change_token != object.change_token || existing.status != object.status
            });
            self.ingestion_repository
                .upsert_object(object.clone())
                .await?;
            self.record_counter_step(
                sync_run_uid,
                Some(object.object_uid),
                "object_change_checked",
                StepOutcome {
                    status: IngestionStepStatus::Skipped,
                    counters: json!({
                        "records_seen": 1,
                        "records_changed": u64::from(changed)
                    }),
                    summary: Some("provider declared record metadata-only".to_string()),
                    retry_count: 0,
                    error_code: None,
                    duration_ms: None,
                },
                KnowledgeSyncCounters {
                    records_seen: 1,
                    records_changed: u64::from(changed),
                    ..KnowledgeSyncCounters::default()
                },
            )
            .await?;
            self.record_step(
                sync_run_uid,
                Some(object.object_uid),
                "content_fetched",
                StepOutcome {
                    status: IngestionStepStatus::Skipped,
                    counters: json!({ "bytes_fetched": 0 }),
                    summary: Some("metadata-only record has no indexable content".to_string()),
                    retry_count: 0,
                    error_code: None,
                    duration_ms: None,
                },
            )
            .await?;
            return Ok(RecordIngestionOutcome::Skipped);
        }

        if let Some(existing) = existing
            && existing.status == crate::domain::ObjectStatus::Active
            && existing.change_token.is_some()
            && existing.change_token == object.change_token
            && self
                .record_has_completed_ingestion(&existing, object.clone(), &record)
                .await?
        {
            self.record_counter_step(
                sync_run_uid,
                Some(existing.object_uid),
                "object_change_checked",
                StepOutcome {
                    status: IngestionStepStatus::Skipped,
                    counters: json!({ "records_seen": 1, "records_changed": 0 }),
                    summary: Some("change token unchanged".to_string()),
                    retry_count: 0,
                    error_code: None,
                    duration_ms: None,
                },
                KnowledgeSyncCounters {
                    records_seen: 1,
                    ..KnowledgeSyncCounters::default()
                },
            )
            .await?;
            return Ok(RecordIngestionOutcome::Skipped);
        }

        self.ingestion_repository
            .upsert_object(object.clone())
            .await?;
        self.record_counter_step(
            sync_run_uid,
            Some(object.object_uid),
            "object_change_checked",
            StepOutcome::completed_with_counters(
                json!({ "records_seen": 1, "records_changed": 1 }),
            ),
            KnowledgeSyncCounters {
                records_seen: 1,
                records_changed: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;

        let input = self
            .resolve_record_parse_input(sync_run_uid, &object, &record)
            .await?;
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "parse_submitted",
            StepOutcome::completed(),
        )
        .await?;
        let parse_span = tracing::info_span!(
            "knowledge_parse_job",
            tenant_id = %object.tenant_id,
            connection_id = %object.connection_uid,
            sync_run_id = %sync_run_uid,
            object_id = %object.object_uid,
            provider = %self.provider,
            parser = %self.parser_label,
            status = tracing::field::Empty,
            error_code = tracing::field::Empty
        );
        let parsed = match self.parse_document(input, &parse_span).await {
            Ok(parsed) => {
                record_span_outcome(&parse_span, "completed", None);
                parsed
            }
            Err(error) => {
                let classification = self
                    .record_failure_step(
                        sync_run_uid,
                        Some(object.object_uid),
                        "parse_completed",
                        &error,
                    )
                    .await?;
                record_span_outcome(&parse_span, "failed", Some(classification.error_code));
                return Err(error);
            }
        };
        self.record_counter_step(
            sync_run_uid,
            Some(object.object_uid),
            "parse_completed",
            StepOutcome::completed_with_counters(
                json!({ "parser_items": parsed.elements.len(), "objects_parsed": 1 }),
            ),
            KnowledgeSyncCounters {
                objects_parsed: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;
        let outcome = self.persist_parsed(sync_run_uid, object, parsed).await?;
        if outcome.ingested {
            Ok(RecordIngestionOutcome::Ingested {
                embeddings_created: outcome.embeddings_created,
            })
        } else {
            Ok(RecordIngestionOutcome::Skipped)
        }
    }

    pub(super) fn materialize_object(
        &self,
        connection_uid: Uuid,
        tenant_id: moa_core::types::identifiers::TenantId,
        record: &ProviderRecord,
    ) -> KnowledgeObject {
        KnowledgeObject {
            object_uid: stable_uid(&format!(
                "knowledge-object:{connection_uid}:{}",
                record.source_id
            )),
            tenant_id,
            connection_uid,
            object_type: record.object_type.clone(),
            source_id: record.source_id.clone(),
            parent_source_id: None,
            source_uri: record.source_uri.clone(),
            title: record.title.clone(),
            change_token: record.change_token.clone(),
            metadata: redact_provider_metadata(record.metadata.clone()),
            status: if record.deleted {
                crate::domain::ObjectStatus::Deleted
            } else if record.materialization.is_metadata_only() {
                // Metadata-only objects retain control-plane metadata and ACLs,
                // but `pending` keeps any previously indexed chunks outside
                // retrieval until the provider returns indexable content again.
                crate::domain::ObjectStatus::Pending
            } else {
                crate::domain::ObjectStatus::Active
            },
            // A newly materialized object has no captured permissions yet. The
            // ACL step replaces this before any content write, so an object that
            // reaches the graph without one stays hidden rather than public.
            acl: crate::domain::ObjectAcl::incomplete(),
            source_updated_at: record.source_updated_at,
            deleted_at: record.deleted.then(Utc::now),
        }
    }

    pub(super) async fn record_has_completed_ingestion(
        &self,
        existing: &KnowledgeObject,
        incoming: KnowledgeObject,
        record: &ProviderRecord,
    ) -> Result<bool> {
        // The object row is advanced before parse and graph writes, so an
        // unchanged change token alone is not completion proof: there must be a
        // completed document version with graph-linked chunks.
        let Some(version) = self
            .ingestion_repository
            .latest_document_version(existing.object_uid)
            .await?
        else {
            return Ok(false);
        };
        // Records carrying real inline text must also match the stored version
        // hash, so an inline edit delivered under an unchanged change token is
        // not skipped. Provider-fetched records have no inline text to hash
        // against the fetched and
        // parsed content, so the unchanged change token plus a completed version
        // is the authority for them.
        if record.materialization.inline_text().is_some() {
            let input = match parse_input_from_record(&self.provider, incoming, record) {
                Ok(input) => input,
                Err(_) => return Ok(false),
            };
            let Some(text) = input.text.as_deref() else {
                return Ok(false);
            };
            let incoming_hash = content_hash(&normalize_text(text));
            if version.content_hash != incoming_hash {
                return Ok(false);
            }
        }
        // The durable materialization fence stays authoritative: a chunk row now
        // always carries its graph occurrence identity (the database enforces
        // `graph_node_uid = chunk_uid`), so identity presence proves nothing about
        // completion. Only the recorded terminal ingestion step does.
        if !self
            .ingestion_repository
            .object_ingestion_completed_since(existing.object_uid, version.created_at)
            .await?
        {
            return Ok(false);
        }
        let chunks = self
            .ingestion_repository
            .chunks_for_version(version.version_uid)
            .await?;
        Ok(!chunks.is_empty())
    }

    /// Resolves the parse input for one provider-normalized materialization intent.
    ///
    /// Inline text and directly fetchable URLs skip the provider hook. A
    /// `ProviderFetch` record requires a configured fetcher and non-empty bytes;
    /// unsupported, empty, or failed fetches are recorded as failures and never
    /// degrade to title content. Metadata-only records are handled before this
    /// method after their object and ACL snapshot are captured.
    pub(super) async fn resolve_record_parse_input(
        &self,
        sync_run_uid: Uuid,
        object: &KnowledgeObject,
        record: &ProviderRecord,
    ) -> Result<ParseInput> {
        if record.materialization.is_metadata_only() {
            return Err(Error::provider(
                &self.provider,
                "metadata-only provider record cannot be parsed",
            ));
        }
        if record.materialization.requires_provider_fetch() {
            let fetched = match &self.content_fetcher {
                Some(fetcher) => fetcher.fetch_record_content(record).await,
                None => Err(Error::provider(
                    &self.provider,
                    "provider record requires content fetch but no fetcher is configured",
                )),
            };
            let content = match fetched {
                Ok(Some(content)) if !content.bytes.is_empty() => content,
                Ok(Some(_)) => {
                    let error = Error::provider(
                        &self.provider,
                        "provider content fetch returned empty bytes",
                    );
                    self.record_failure_step(
                        sync_run_uid,
                        Some(object.object_uid),
                        "content_fetched",
                        &error,
                    )
                    .await?;
                    return Err(error);
                }
                Ok(None) => {
                    let error = Error::provider(
                        &self.provider,
                        "provider does not support the record's required content fetch",
                    );
                    self.record_failure_step(
                        sync_run_uid,
                        Some(object.object_uid),
                        "content_fetched",
                        &error,
                    )
                    .await?;
                    return Err(error);
                }
                Err(error) => {
                    self.record_failure_step(
                        sync_run_uid,
                        Some(object.object_uid),
                        "content_fetched",
                        &error,
                    )
                    .await?;
                    return Err(error);
                }
            };
            let input = parse_input_from_fetched_content(object.clone(), record, content);
            self.record_resolved_parse_input(sync_run_uid, object, input)
                .await
        } else {
            let input = parse_input_from_record(&self.provider, object.clone(), record)?;
            self.record_resolved_parse_input(sync_run_uid, object, input)
                .await
        }
    }

    /// Records the `content_fetched` step for a resolved parse input.
    ///
    pub(super) async fn record_resolved_parse_input(
        &self,
        sync_run_uid: Uuid,
        object: &KnowledgeObject,
        input: ParseInput,
    ) -> Result<ParseInput> {
        let bytes_fetched = input.bytes.as_ref().map_or_else(
            || input.text.as_ref().map_or(0, |text| text.len()),
            Vec::len,
        );
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "content_fetched",
            StepOutcome::completed_with_counters(json!({ "bytes_fetched": bytes_fetched })),
        )
        .await?;
        Ok(input)
    }
}

/// Builds a [`ParseInput`] from one explicit provider-normalized materialization intent.
///
/// # Errors
///
/// Returns [`Error::Provider`] for provider-fetch and metadata-only intents,
/// which require pipeline-level handling instead of direct parser submission.
pub fn parse_input_from_record(
    provider: &str,
    object: KnowledgeObject,
    record: &ProviderRecord,
) -> Result<ParseInput> {
    let (text, source_url) = if let Some(text) = record.materialization.inline_text() {
        (Some(text.to_owned()), None)
    } else if let Some(url) = record.materialization.fetchable_url() {
        (None, Some(url.to_owned()))
    } else if record.materialization.requires_provider_fetch() {
        return Err(Error::provider(
            provider,
            "provider-fetch record requires the content-fetch pipeline",
        ));
    } else {
        return Err(Error::provider(
            provider,
            "metadata-only provider record has no indexable content",
        ));
    };
    Ok(ParseInput {
        object,
        file_name: record.title.clone(),
        mime_type: record.materialization.mime_type().map(ToOwned::to_owned),
        source_url,
        bytes: None,
        text,
        options: json!({}),
    })
}

/// Builds a [`ParseInput`] from provider-fetched byte content.
///
/// The fetched bytes route through the parser-selection heuristic exactly like a
/// downloaded document: text bytes fall to the native parser while binary bytes
/// go to the configured external parser. The MIME type prefers the value
/// reported by the fetch, falling back to any MIME field on the record.
fn parse_input_from_fetched_content(
    object: KnowledgeObject,
    record: &ProviderRecord,
    content: FetchedRecordContent,
) -> ParseInput {
    ParseInput {
        object,
        file_name: record.title.clone(),
        mime_type: content
            .mime_type
            .or_else(|| record.materialization.mime_type().map(ToOwned::to_owned)),
        source_url: None,
        bytes: Some(content.bytes),
        text: None,
        options: json!({}),
    }
}

/// Returns whether a parse input should fall back to the native parser.
///
/// True only when the input carries neither bytes nor a `source_url` (nothing
/// for an external parser to fetch or upload) and the configured parser is an
/// external document parser. Inputs with bytes or a URL, and non-external
/// configured parsers (including `native` and test parsers), keep the
/// configured parser.
fn use_native_document_fallback(input: &ParseInput, parser_label: &str) -> bool {
    input.bytes.is_none()
        && input.source_url.is_none()
        && crate::parser::is_external_document_parser(parser_label)
}

#[cfg(test)]
mod tests {
    use moa_core::types::identifiers::TenantId;

    use super::{
        ParseInput, parse_input_from_fetched_content, parse_input_from_record,
        use_native_document_fallback,
    };
    use crate::domain::{
        FetchedRecordContent, KnowledgeObject, ObjectStatus, ProviderRecord, ProviderRecordAcl,
        ProviderRecordMaterialization,
    };
    use serde_json::json;
    use uuid::Uuid;

    fn object() -> KnowledgeObject {
        KnowledgeObject {
            acl: crate::domain::ObjectAcl::incomplete(),
            object_uid: Uuid::from_u128(1),
            tenant_id: TenantId::from(Uuid::from_u128(2)),
            connection_uid: Uuid::from_u128(3),
            object_type: "document".to_string(),
            source_id: "src-1".to_string(),
            parent_source_id: None,
            source_uri: None,
            title: None,
            change_token: None,
            metadata: json!({}),
            status: ObjectStatus::Pending,
            source_updated_at: None,
            deleted_at: None,
        }
    }

    fn record(
        title: Option<&str>,
        source_uri: Option<&str>,
        materialization: ProviderRecordMaterialization,
    ) -> ProviderRecord {
        ProviderRecord {
            acl: ProviderRecordAcl {
                provider_revision: "fixture-acl-rev".to_string(),
                complete: true,
                entries: Vec::new(),
            },
            source_id: "src-1".to_string(),
            object_type: "document".to_string(),
            title: title.map(ToString::to_string),
            source_uri: source_uri.map(ToString::to_string),
            change_token: None,
            deleted: false,
            source_updated_at: None,
            materialization,
            metadata: json!({}),
            payload: json!({ "ignored_by_ingestion": "display metadata" }),
        }
    }

    fn parse_input(
        bytes: Option<Vec<u8>>,
        source_url: Option<&str>,
        text: Option<&str>,
    ) -> ParseInput {
        ParseInput {
            object: object(),
            file_name: None,
            mime_type: None,
            source_url: source_url.map(ToString::to_string),
            bytes,
            text: text.map(ToString::to_string),
            options: json!({}),
        }
    }

    #[test]
    fn parse_input_uses_inline_content_as_text() {
        // Pins: inline body text materializes as ParseInput.text with no
        // source_url, so no external fetch is needed even when a web link exists.
        let record = record(
            Some("Doc"),
            Some("https://web.example/doc"),
            ProviderRecordMaterialization::InlineText {
                text: "hello world".to_string(),
                mime_type: Some("text/plain".to_string()),
            },
        );
        let input = parse_input_from_record("nango", object(), &record).expect("materializes");
        assert_eq!(input.text.as_deref(), Some("hello world"));
        assert_eq!(input.source_url, None);
        assert_eq!(input.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(input.file_name.as_deref(), Some("Doc"));
    }

    #[test]
    fn parse_input_sets_source_url_for_url_only_record() {
        // Pins: a record with a download URL but no inline text yields a
        // source_url for an external parser and leaves text unset.
        let record = record(
            Some("Report.pdf"),
            None,
            ProviderRecordMaterialization::FetchableUrl {
                url: "https://files.example/report.pdf".to_string(),
                mime_type: Some("application/pdf".to_string()),
            },
        );
        let input = parse_input_from_record("nango", object(), &record).expect("materializes");
        assert_eq!(
            input.source_url.as_deref(),
            Some("https://files.example/report.pdf")
        );
        assert_eq!(input.text, None);
        assert_eq!(input.mime_type.as_deref(), Some("application/pdf"));
    }

    #[test]
    fn parse_input_rejects_metadata_only_even_when_title_exists() {
        // Pins: display metadata is never substituted for document content.
        let record = record(
            Some("Just A Title"),
            Some("https://web.example/x"),
            ProviderRecordMaterialization::MetadataOnly,
        );
        let error = parse_input_from_record("nango", object(), &record)
            .expect_err("metadata-only record must not parse");
        assert!(error.to_string().contains("metadata-only"));
    }

    #[test]
    fn parse_input_rejects_provider_fetch_before_content_is_downloaded() {
        // Pins: provider-fetch intent must go through the authenticated fetcher;
        // direct parser materialization cannot guess from payload or title.
        let record = record(
            Some("Display Title"),
            Some("https://web.example/x"),
            ProviderRecordMaterialization::ProviderFetch {
                mime_type: Some("application/pdf".to_string()),
            },
        );
        let error = parse_input_from_record("nango", object(), &record)
            .expect_err("unfetched provider content must not parse");
        assert!(error.to_string().contains("content-fetch pipeline"));
    }

    #[test]
    fn fetched_content_builds_bytes_backed_parse_input() {
        // Pins: fetched byte content becomes ParseInput.bytes (never text or
        // source_url), and the fetch-reported MIME is preferred over the record's
        // own MIME field.
        let record = record(
            Some("Report"),
            None,
            ProviderRecordMaterialization::ProviderFetch {
                mime_type: Some("application/pdf".to_string()),
            },
        );
        let content = FetchedRecordContent {
            bytes: b"fetched-bytes".to_vec(),
            mime_type: Some("text/plain".to_string()),
        };
        let input = parse_input_from_fetched_content(object(), &record, content);
        assert_eq!(input.bytes.as_deref(), Some(b"fetched-bytes".as_slice()));
        assert_eq!(input.text, None);
        assert_eq!(input.source_url, None);
        assert_eq!(input.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(input.file_name.as_deref(), Some("Report"));
    }

    #[test]
    fn native_fallback_only_for_text_only_external_parser() {
        // Pins: text-only inputs override an external parser to native, while
        // inputs with bytes or a URL, and non-external parsers, keep the
        // configured parser.
        let text_only = parse_input(None, None, Some("inline"));
        assert!(use_native_document_fallback(&text_only, "llamaparse"));
        assert!(use_native_document_fallback(&text_only, "unstructured"));
        assert!(use_native_document_fallback(&text_only, "reducto"));
        assert!(!use_native_document_fallback(&text_only, "native"));
        assert!(!use_native_document_fallback(&text_only, "test_parser"));

        let with_url = parse_input(None, Some("https://files.example/x.pdf"), None);
        assert!(!use_native_document_fallback(&with_url, "llamaparse"));

        let with_bytes = parse_input(Some(vec![1, 2, 3]), None, None);
        assert!(!use_native_document_fallback(&with_bytes, "llamaparse"));
    }
}
