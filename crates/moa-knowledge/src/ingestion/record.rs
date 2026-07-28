//! Knowledge ingestion record operations.

use super::steps::record_span_outcome;
use super::*;

impl<R, P, E, G> KnowledgeIngestionPipeline<R, P, E, G>
where
    R: KnowledgeRepository,
    P: DocumentParser,
    E: EmbeddingProvider,
    G: KnowledgeGraphWriter,
{
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
            .repository
            .get_object_by_source(object.connection_uid, &object.source_id)
            .await?;

        // The object row must exist before its ACL snapshot can reference it. A
        // brand-new object lands `incomplete` — invisible — and only the capture
        // below can make it readable.
        if existing.is_none() {
            self.repository.upsert_object(object.clone()).await?;
        }
        // Ahead of BOTH content fences: an unshared folder must stop being
        // retrievable on the next sync pass even though not one byte changed,
        // and re-parsing a document to learn that is pure waste.
        self.capture_record_acl(sync_run_uid, &object, &record)
            .await?;

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

        self.repository.upsert_object(object.clone()).await?;
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
            .repository
            .latest_document_version(existing.object_uid)
            .await?
        else {
            return Ok(false);
        };
        // Records carrying real inline text must also match the stored version
        // hash, so an inline edit delivered under an unchanged change token is
        // not skipped. Records that rely on the content-fetch hook or the
        // title-only fallback have no inline text to hash against the
        // fetched/parsed content — hashing their title would never match and
        // would force a re-fetch every sync — so the unchanged change token plus
        // a completed version is the authority for them.
        if record_materializes_inline(record) {
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
            .repository
            .object_ingestion_completed_since(existing.object_uid, version.created_at)
            .await?
        {
            return Ok(false);
        }
        let chunks = self
            .repository
            .chunks_for_version(version.version_uid)
            .await?;
        Ok(!chunks.is_empty())
    }

    /// Resolves the parse input for one record, downloading provider content
    /// when the record carries neither inline text nor a fetchable URL.
    ///
    /// Records that already materialize (inline text or a directly fetchable
    /// URL) skip the fetch entirely. Otherwise, when a content fetcher is wired,
    /// a successful fetch yields a bytes-backed [`ParseInput`]; a fetch that
    /// returns nothing or errors records a distinct soft signal and falls back
    /// to the title-only behavior. The `content_fetched` step is recorded here
    /// exactly once, and a record with no title still fails with the pinned
    /// `materializable text` classification.
    pub(super) async fn resolve_record_parse_input(
        &self,
        sync_run_uid: Uuid,
        object: &KnowledgeObject,
        record: &ProviderRecord,
    ) -> Result<ParseInput> {
        if record_has_materializable_content(record) {
            let input = parse_input_from_record(&self.provider, object.clone(), record)?;
            return self
                .record_resolved_parse_input(sync_run_uid, object, input, None)
                .await;
        }

        let mut fetch_note: Option<&'static str> = None;
        if let Some(fetcher) = &self.content_fetcher {
            match fetcher.fetch_record_content(record).await {
                Ok(Some(content)) if !content.bytes.is_empty() => {
                    let input = parse_input_from_fetched_content(object.clone(), record, content);
                    return self
                        .record_resolved_parse_input(sync_run_uid, object, input, None)
                        .await;
                }
                Ok(_) => {
                    fetch_note = Some("provider_content_fetch_empty");
                }
                Err(error) => {
                    fetch_note = Some("provider_content_fetch_failed");
                    tracing::warn!(
                        sync_run_id = %sync_run_uid,
                        object_id = %object.object_uid,
                        provider = %self.provider,
                        error = %error,
                        "provider content fetch failed; falling back to record title"
                    );
                }
            }
        }

        let input = match parse_input_from_record(&self.provider, object.clone(), record) {
            Ok(input) => input,
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
        self.record_resolved_parse_input(sync_run_uid, object, input, fetch_note)
            .await
    }

    /// Records the `content_fetched` step for a resolved parse input.
    ///
    /// A `fetch_note` marks a soft content-fetch fallback: the step still
    /// completes (the title-only input is usable) but carries a distinct
    /// `error_code` so operators can tell "content fetch failed" apart from a
    /// plain metadata-only record.
    pub(super) async fn record_resolved_parse_input(
        &self,
        sync_run_uid: Uuid,
        object: &KnowledgeObject,
        input: ParseInput,
        fetch_note: Option<&'static str>,
    ) -> Result<ParseInput> {
        let bytes_fetched = input.bytes.as_ref().map_or_else(
            || input.text.as_ref().map_or(0, |text| text.len()),
            Vec::len,
        );
        let outcome = match fetch_note {
            Some(note) => StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({ "bytes_fetched": bytes_fetched }),
                summary: Some(
                    "provider content fetch unavailable; indexed record title".to_string(),
                ),
                retry_count: 0,
                error_code: Some(note.to_string()),
                duration_ms: None,
            },
            None => StepOutcome::completed_with_counters(json!({ "bytes_fetched": bytes_fetched })),
        };
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "content_fetched",
            outcome,
        )
        .await?;
        Ok(input)
    }
}

/// Payload fields, in priority order, that carry already-materialized record
/// text. The first present string is used as inline `text` for the parser.
const RECORD_INLINE_TEXT_FIELDS: &[&str] = &[
    "text",
    "content",
    "body",
    "plain_text",
    "plaintext",
    "markdown",
    "html",
];

/// Payload fields, in priority order, that carry a directly fetchable document
/// URL. The first present string becomes `source_url` so an external parser can
/// download the file.
///
/// These are download/content links only. Auth-walled browser viewers such as
/// Google Drive's `webViewLink`/`web_view_link`, and the ambiguous generic
/// `url` (which providers map to the human-facing `source_uri`), are
/// deliberately excluded: they are not fetchable by an unauthenticated parser,
/// so a record carrying only such a link routes to the provider content-fetch
/// hook or the title-only fallback instead of a doomed download.
const RECORD_SOURCE_URL_FIELDS: &[&str] = &[
    "download_url",
    "file_url",
    "content_url",
    "web_content_link",
    "webContentLink",
];

/// Metadata and payload fields, in priority order, that carry a MIME type.
const RECORD_MIME_TYPE_FIELDS: &[&str] = &["mime_type", "mimeType", "content_type", "contentType"];

/// Builds a [`ParseInput`] from a normalized provider record using a
/// provider-agnostic payload convention shared by every
/// [`LinkedIntegrationProvider`](crate::providers::LinkedIntegrationProvider)
/// adapter.
///
/// Resolution order:
///
/// 1. Inline text from the first present of [`RECORD_INLINE_TEXT_FIELDS`] is
///    used directly as `text` (no fetch or upload needed).
/// 2. Otherwise `source_url` is populated from the first present of
///    [`RECORD_SOURCE_URL_FIELDS`] so an external document parser can fetch the
///    file, and any [`RECORD_MIME_TYPE_FIELDS`] value is passed through.
/// 3. Otherwise the record `title` is indexed as `text`, preserving the prior
///    title-only fallback behavior.
///
/// # Errors
///
/// Returns [`Error::Provider`] when a record carries no inline text, no
/// fetchable URL, and no title. The message retains the `materializable text`
/// marker used by failure classification.
pub fn parse_input_from_record(
    provider: &str,
    object: KnowledgeObject,
    record: &ProviderRecord,
) -> Result<ParseInput> {
    let inline_text = first_record_string(record, RECORD_INLINE_TEXT_FIELDS);
    let source_url = if inline_text.is_none() {
        first_record_string(record, RECORD_SOURCE_URL_FIELDS)
    } else {
        None
    };
    let text = match (&inline_text, &source_url) {
        (Some(_), _) => inline_text,
        (None, Some(_)) => None,
        (None, None) => Some(record.title.clone().ok_or_else(|| {
            Error::Provider {
                provider: provider.to_string(),
                message:
                    "provider record did not include materializable text or a fetchable source URL"
                        .to_string(),
            }
        })?),
    };
    Ok(ParseInput {
        object,
        file_name: record.title.clone(),
        mime_type: first_record_string(record, RECORD_MIME_TYPE_FIELDS),
        source_url,
        bytes: None,
        text,
        options: json!({}),
    })
}

/// Returns whether a record carries inline text materialized directly from its
/// own payload fields.
///
/// This is the signal distinguishing records whose stored content is the record
/// text (so the version hash is meaningful for change detection) from records
/// whose content comes from the fetch hook or the title-only fallback (where the
/// change token, not a title hash, is the completion authority).
fn record_materializes_inline(record: &ProviderRecord) -> bool {
    first_record_string(record, RECORD_INLINE_TEXT_FIELDS).is_some()
}

/// Returns whether a record already materializes without a provider content
/// fetch, i.e. it carries inline text or a directly fetchable source URL.
fn record_has_materializable_content(record: &ProviderRecord) -> bool {
    record_materializes_inline(record)
        || first_record_string(record, RECORD_SOURCE_URL_FIELDS).is_some()
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
            .or_else(|| first_record_string(record, RECORD_MIME_TYPE_FIELDS)),
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

/// Returns the first of `keys` present as a non-empty string in the record
/// payload, then metadata. Payload wins because it holds the raw source fields.
fn first_record_string(record: &ProviderRecord, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        record
            .payload
            .get(*key)
            .or_else(|| record.metadata.get(*key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

#[cfg(test)]
mod tests {
    use moa_core::types::identifiers::TenantId;

    use super::{
        ParseInput, parse_input_from_fetched_content, parse_input_from_record,
        record_has_materializable_content, use_native_document_fallback,
    };
    use crate::domain::{FetchedRecordContent, KnowledgeObject, ObjectStatus, ProviderRecord};
    use serde_json::{Value, json};
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

    fn record(title: Option<&str>, source_uri: Option<&str>, payload: Value) -> ProviderRecord {
        ProviderRecord {
            acl: crate::domain::RecordAcl::UniformlyPublic,
            source_id: "src-1".to_string(),
            object_type: "document".to_string(),
            title: title.map(ToString::to_string),
            source_uri: source_uri.map(ToString::to_string),
            change_token: None,
            deleted: false,
            source_updated_at: None,
            metadata: json!({}),
            payload,
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
            json!({ "content": "hello world", "mime_type": "text/plain" }),
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
            json!({
                "download_url": "https://files.example/report.pdf",
                "mime_type": "application/pdf"
            }),
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
    fn parse_input_falls_back_to_title_when_no_body_or_url() {
        // Pins: prior title-only behavior. A record with neither inline text nor
        // a fetchable download URL indexes its title as text; the human-facing
        // source_uri web link is not treated as a fetchable source_url.
        let record = record(
            Some("Just A Title"),
            Some("https://web.example/x"),
            json!({ "irrelevant": "field" }),
        );
        let input = parse_input_from_record("nango", object(), &record).expect("materializes");
        assert_eq!(input.text.as_deref(), Some("Just A Title"));
        assert_eq!(input.source_url, None);
    }

    #[test]
    fn parse_input_errors_without_text_url_or_title() {
        // Pins: a record with no inline text, no fetchable URL, and no title
        // fails with the `materializable text` classification marker.
        let record = record(
            None,
            Some("https://web.example/x"),
            json!({ "safe": "meta" }),
        );
        let error = parse_input_from_record("nango", object(), &record).expect_err("no content");
        let message = error.to_string();
        assert!(
            message.contains("materializable text"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn parse_input_ignores_auth_walled_web_view_link() {
        // Pins the web_view_link hazard fix: a record whose only link is an
        // auth-walled Google Drive browser viewer (webViewLink/web_view_link) or
        // a generic `url` is not treated as a fetchable source_url. With a title
        // it falls back to title-only text; the viewer link never routes to a
        // doomed unauthenticated download.
        for field in ["web_view_link", "webViewLink", "url"] {
            let record = record(
                Some("Drive Doc"),
                None,
                json!({ field: "https://drive.google.com/file/d/abc/view" }),
            );
            let input = parse_input_from_record("nango", object(), &record).expect("materializes");
            assert_eq!(
                input.source_url, None,
                "field `{field}` must not be fetchable"
            );
            assert_eq!(input.text.as_deref(), Some("Drive Doc"));
            assert!(
                !record_has_materializable_content(&record),
                "field `{field}` must not count as materializable content"
            );
        }
    }

    #[test]
    fn parse_input_still_accepts_genuine_download_links() {
        // Pins: real download/content links remain fetchable after the
        // web_view_link fix removed the auth-walled viewer candidates.
        for field in [
            "download_url",
            "file_url",
            "content_url",
            "web_content_link",
            "webContentLink",
        ] {
            let record = record(
                Some("File"),
                None,
                json!({ field: "https://files.example/x" }),
            );
            let input = parse_input_from_record("nango", object(), &record).expect("materializes");
            assert_eq!(
                input.source_url.as_deref(),
                Some("https://files.example/x"),
                "field `{field}` should remain fetchable"
            );
            assert!(
                record_has_materializable_content(&record),
                "field `{field}` should count as materializable content"
            );
        }
    }

    #[test]
    fn fetched_content_builds_bytes_backed_parse_input() {
        // Pins: fetched byte content becomes ParseInput.bytes (never text or
        // source_url), and the fetch-reported MIME is preferred over the record's
        // own MIME field.
        let record = record(
            Some("Report"),
            None,
            json!({ "mime_type": "application/pdf" }),
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
