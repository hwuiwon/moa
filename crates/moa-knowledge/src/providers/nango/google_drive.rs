//! Google Drive content-fetch strategy for the Nango proxy.
//!
//! Everything Drive-specific lives here: which Drive API path and query fetch a
//! record's bytes, and how Google Workspace editor files are exported. The
//! parent module's registry routes `google-drive` connections to
//! [`content_fetch_plan`]; the generic proxy executor stays integration-agnostic.

use super::ProxyFetchPlan;
use crate::{domain::ProviderRecord, providers::http::string_field};

/// MIME-type prefix identifying Google Workspace editor files (Docs, Sheets,
/// Slides) that must be exported rather than downloaded verbatim.
const GOOGLE_APPS_MIME_PREFIX: &str = "application/vnd.google-apps.";

/// Builds the proxy fetch plan for one Google Drive record, or `None` when the
/// record has no fetchable content (missing id, or a google-apps type with no
/// text export such as folders, drawings, forms, and shortcuts).
///
/// Google Workspace editor files are exported to a concrete text format via
/// `files/{id}/export`; every other Drive file streams verbatim via
/// `files/{id}?alt=media`.
pub(super) fn content_fetch_plan(record: &ProviderRecord) -> Option<ProxyFetchPlan> {
    let file_id = record.source_id.trim();
    if file_id.is_empty() {
        return None;
    }
    let source_mime = string_field(&record.payload, &["mimeType", "mime_type"])
        .or_else(|| string_field(&record.metadata, &["mimeType", "mime_type"]));

    if let Some(mime) = source_mime.as_deref()
        && mime.starts_with(GOOGLE_APPS_MIME_PREFIX)
    {
        let export_mime = drive_export_mime(mime)?;
        return Some(ProxyFetchPlan {
            path_segments: drive_file_segments(file_id, true),
            query: vec![("mimeType".to_string(), export_mime.to_string())],
            result_mime: Some(export_mime.to_string()),
            fallback_mime: None,
        });
    }

    Some(ProxyFetchPlan {
        path_segments: drive_file_segments(file_id, false),
        query: vec![("alt".to_string(), "media".to_string())],
        result_mime: None,
        fallback_mime: source_mime,
    })
}

/// Returns the text-oriented export MIME for a Google Workspace editor file, or
/// `None` when the google-apps type has no plain-text export.
///
/// Google Drive rejects `export?mimeType=text/plain` for spreadsheets (which
/// export to `text/csv`) and for non-document types entirely, so the target is
/// chosen per subtype instead of a single format.
fn drive_export_mime(google_apps_mime: &str) -> Option<&'static str> {
    match google_apps_mime {
        "application/vnd.google-apps.document" | "application/vnd.google-apps.presentation" => {
            Some("text/plain")
        }
        "application/vnd.google-apps.spreadsheet" => Some("text/csv"),
        _ => None,
    }
}

/// Builds the `drive/v3/files/{id}` path segments, appending `export` when the
/// file must be exported rather than downloaded verbatim.
fn drive_file_segments(file_id: &str, export: bool) -> Vec<String> {
    let mut segments = vec![
        "drive".to_string(),
        "v3".to_string(),
        "files".to_string(),
        file_id.to_string(),
    ];
    if export {
        segments.push("export".to_string());
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::content_fetch_plan;
    use crate::domain::ProviderRecord;
    use serde_json::{Value, json};

    fn record(source_id: &str, payload: Value) -> ProviderRecord {
        ProviderRecord {
            source_id: source_id.to_string(),
            object_type: "drive_file".to_string(),
            title: Some("Doc".to_string()),
            source_uri: None,
            change_token: None,
            deleted: false,
            source_updated_at: None,
            metadata: json!({}),
            payload,
        }
    }

    #[test]
    fn google_docs_and_slides_export_as_plain_text() {
        // Pins: editor docs export via files/{id}/export?mimeType=text/plain with
        // text/plain as the authoritative result MIME.
        for mime in [
            "application/vnd.google-apps.document",
            "application/vnd.google-apps.presentation",
        ] {
            let plan = content_fetch_plan(&record("doc-1", json!({ "mimeType": mime })))
                .expect("editor doc should have a fetch plan");
            assert_eq!(
                plan.path_segments,
                vec!["drive", "v3", "files", "doc-1", "export"]
            );
            assert_eq!(
                plan.query,
                vec![("mimeType".to_string(), "text/plain".to_string())]
            );
            assert_eq!(plan.result_mime.as_deref(), Some("text/plain"));
            assert_eq!(plan.fallback_mime, None);
        }
    }

    #[test]
    fn spreadsheets_export_as_csv() {
        // Pins: Sheets export to text/csv (text/plain is rejected by Drive).
        let plan = content_fetch_plan(&record(
            "sheet-1",
            json!({ "mimeType": "application/vnd.google-apps.spreadsheet" }),
        ))
        .expect("spreadsheet should have a fetch plan");
        assert_eq!(
            plan.query,
            vec![("mimeType".to_string(), "text/csv".to_string())]
        );
        assert_eq!(plan.result_mime.as_deref(), Some("text/csv"));
    }

    #[test]
    fn non_text_exportable_google_apps_types_have_no_plan() {
        // Pins: folders (and similar non-text google-apps types) are not
        // fetchable, so no request is planned.
        assert!(
            content_fetch_plan(&record(
                "folder-1",
                json!({ "mimeType": "application/vnd.google-apps.folder" }),
            ))
            .is_none()
        );
    }

    #[test]
    fn binary_files_stream_via_alt_media() {
        // Pins: a regular file downloads verbatim with alt=media, deferring the
        // MIME to the response (falling back to the record's own MIME).
        let plan = content_fetch_plan(&record("bin-1", json!({ "mimeType": "application/pdf" })))
            .expect("binary file should have a fetch plan");
        assert_eq!(plan.path_segments, vec!["drive", "v3", "files", "bin-1"]);
        assert_eq!(plan.query, vec![("alt".to_string(), "media".to_string())]);
        assert_eq!(plan.result_mime, None);
        assert_eq!(plan.fallback_mime.as_deref(), Some("application/pdf"));
    }

    #[test]
    fn missing_file_id_has_no_plan() {
        // Pins: a blank source id cannot address a Drive file.
        assert!(
            content_fetch_plan(&record("  ", json!({ "mimeType": "application/pdf" }))).is_none()
        );
    }
}
