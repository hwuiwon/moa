//! Contact session message parsing, upload validation, and attachment helpers.

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Multipart, Request};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use moa_core::{
    Attachment, ContactSessionMessageRequest, MAX_CONTACT_SESSION_ATTACHMENT_BYTES,
    MAX_CONTACT_SESSION_ATTACHMENT_NAME_BYTES, MAX_CONTACT_SESSION_ATTACHMENT_TOTAL_BYTES,
    MAX_CONTACT_SESSION_ATTACHMENTS_PER_MESSAGE, MoaError, SessionAttachmentStore, SessionId,
    SessionStore, TenantId, normalize_contact_session_photo_mime,
    validate_contact_session_message_text,
};
use uuid::Uuid;

use super::AppState;

const MAX_SESSION_PHOTO_DIMENSION: u32 = 12_000;
const MAX_SESSION_PHOTO_PIXELS: u64 = 25_000_000;

pub(super) struct SessionMessageInput {
    pub(super) message: ContactSessionMessageRequest,
    pub(super) uploads: Vec<SessionAttachmentUpload>,
}

pub(super) struct SessionAttachmentUpload {
    name: String,
    mime_type: String,
    content: Vec<u8>,
}

#[derive(Debug)]
pub(super) struct SessionMessageRequestError {
    pub(super) status: StatusCode,
    pub(super) message: &'static str,
}

impl SessionMessageRequestError {
    fn bad_request(message: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message,
        }
    }
}

pub(super) fn authorization_bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then(|| token.to_string())
}

pub(super) fn attachment_response(attachment: &Attachment, content: Vec<u8>) -> Response {
    let content_len = content.len();
    let mut builder = Response::builder().status(StatusCode::OK);
    if let Some(mime_type) = attachment.mime_type.as_deref()
        && let Ok(value) = HeaderValue::from_str(mime_type)
    {
        builder = builder.header(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&content_len.to_string()) {
        builder = builder.header(header::CONTENT_LENGTH, value);
    }
    match builder.body(Body::from(content)) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(error = %error, "build attachment response failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "response build failed").into_response()
        }
    }
}

pub(super) async fn session_message_input(
    session_id: Uuid,
    headers: &HeaderMap,
    request: Request,
    state: &AppState,
) -> Result<SessionMessageInput, SessionMessageRequestError> {
    if is_multipart_content_type(headers) {
        return multipart_session_message_request(session_id, request, state).await;
    }

    let body = Bytes::from_request(request, state)
        .await
        .map_err(|_| SessionMessageRequestError::bad_request("bad session message body"))?;
    contact_session_message_request(session_id, &body)
        .map(|message| SessionMessageInput {
            message,
            uploads: Vec::new(),
        })
        .map_err(SessionMessageRequestError::bad_request)
}

pub(super) async fn persist_session_attachments(
    state: &AppState,
    message: &ContactSessionMessageRequest,
    uploads: Vec<SessionAttachmentUpload>,
) -> Result<Vec<Attachment>, MoaError> {
    let session = state.session_store.get_session(message.session_id).await?;
    if session.tenant_id != message.tenant_id {
        return Err(MoaError::StorageError(format!(
            "session `{}` does not belong to tenant `{}`",
            message.session_id, message.tenant_id
        )));
    }
    let contact_id = session.contact.as_ref().map(|contact| contact.contact_id);
    let mut attachments = Vec::with_capacity(uploads.len());
    for upload in uploads {
        let attachment = match state
            .session_store
            .put(
                message.tenant_id,
                message.session_id,
                contact_id,
                upload.name,
                upload.mime_type,
                upload.content,
            )
            .await
        {
            Ok(attachment) => attachment,
            Err(error) => {
                cleanup_session_attachments(
                    state,
                    message.tenant_id,
                    message.session_id,
                    &attachments,
                )
                .await;
                return Err(error);
            }
        };
        attachments.push(attachment);
    }
    Ok(attachments)
}

pub(super) async fn cleanup_session_attachments(
    state: &AppState,
    tenant_id: TenantId,
    session_id: SessionId,
    attachments: &[Attachment],
) {
    for attachment in attachments {
        let Some(attachment_id) = attachment.id else {
            continue;
        };
        if let Err(error) = state
            .session_store
            .delete(tenant_id, session_id, attachment_id)
            .await
        {
            tracing::warn!(
                %error,
                %session_id,
                %attachment_id,
                "failed to clean up session attachment after message rejection"
            );
        }
    }
}

fn contact_session_message_request(
    session_id: Uuid,
    body: &Bytes,
) -> Result<ContactSessionMessageRequest, &'static str> {
    let mut value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| "bad session message body")?;
    let Some(object) = value.as_object_mut() else {
        return Err("session message body must be object");
    };
    object.insert("session_id".to_string(), serde_json::json!(session_id));
    let message: ContactSessionMessageRequest =
        serde_json::from_value(value).map_err(|_| "bad session message body")?;
    if !message.attachments.is_empty() {
        return Err("session message attachments must be uploaded as multipart");
    }
    message.validate_admitted_payload()?;
    Ok(message)
}

async fn multipart_session_message_request(
    session_id: Uuid,
    request: Request,
    state: &AppState,
) -> Result<SessionMessageInput, SessionMessageRequestError> {
    let mut multipart = Multipart::from_request(request, state)
        .await
        .map_err(|_| SessionMessageRequestError::bad_request("bad multipart session message"))?;
    let mut tenant_id = None;
    let mut contact_token = None;
    let mut user_message = String::new();
    let mut model = None;
    let mut max_turns = None;
    let mut uploads = Vec::new();
    let mut total_upload_bytes = 0_usize;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| SessionMessageRequestError::bad_request("bad multipart session message"))?
    {
        let field_name = field.name().unwrap_or_default().to_string();
        let file_name = field.file_name().map(ToOwned::to_owned);
        let declared_mime = field.content_type().map(ToString::to_string);
        let bytes = field
            .bytes()
            .await
            .map_err(|_| SessionMessageRequestError::bad_request("bad multipart session part"))?;

        if file_name.is_some() || is_upload_field(&field_name) {
            if bytes.is_empty() {
                return Err(SessionMessageRequestError::bad_request(
                    "photo upload was empty",
                ));
            }
            if uploads.len() >= MAX_CONTACT_SESSION_ATTACHMENTS_PER_MESSAGE {
                return Err(SessionMessageRequestError::bad_request(
                    "too many photo uploads",
                ));
            }
            if bytes.len() > MAX_CONTACT_SESSION_ATTACHMENT_BYTES {
                return Err(SessionMessageRequestError::bad_request(
                    "photo upload is too large",
                ));
            }
            total_upload_bytes = total_upload_bytes.saturating_add(bytes.len());
            if total_upload_bytes > MAX_CONTACT_SESSION_ATTACHMENT_TOTAL_BYTES {
                return Err(SessionMessageRequestError::bad_request(
                    "photo uploads are too large",
                ));
            }
            let mime_type = canonical_photo_mime(declared_mime.as_deref(), &bytes)?;
            let name = validated_upload_name(file_name.as_deref())?;
            uploads.push(SessionAttachmentUpload {
                name,
                mime_type: mime_type.to_string(),
                content: bytes.to_vec(),
            });
            continue;
        }

        let value = String::from_utf8(bytes.to_vec()).map_err(|_| {
            SessionMessageRequestError::bad_request("multipart text field was not utf-8")
        })?;
        match field_name.as_str() {
            "tenant_id" => {
                let parsed = Uuid::parse_str(value.trim())
                    .map_err(|_| SessionMessageRequestError::bad_request("bad tenant_id"))?;
                tenant_id = Some(TenantId::from(parsed));
            }
            "contact_token" => contact_token = Some(value),
            "user_message" | "text" | "message" => {
                validate_contact_session_message_text(&value)
                    .map_err(SessionMessageRequestError::bad_request)?;
                user_message = value;
            }
            "model" if !value.trim().is_empty() => model = Some(value),
            "max_turns" if !value.trim().is_empty() => {
                max_turns = Some(
                    value
                        .trim()
                        .parse::<u32>()
                        .map_err(|_| SessionMessageRequestError::bad_request("bad max_turns"))?,
                );
            }
            _ => {}
        }
    }

    if user_message.trim().is_empty() && uploads.is_empty() {
        return Err(SessionMessageRequestError::bad_request(
            "session message requires text or a photo",
        ));
    }

    Ok(SessionMessageInput {
        message: ContactSessionMessageRequest {
            tenant_id: tenant_id
                .ok_or_else(|| SessionMessageRequestError::bad_request("tenant_id is required"))?,
            session_id: SessionId(session_id),
            contact_token: contact_token.ok_or_else(|| {
                SessionMessageRequestError::bad_request("contact_token is required")
            })?,
            user_message,
            attachments: Vec::new(),
            model,
            max_turns,
        },
        uploads,
    })
}

fn is_multipart_content_type(headers: &HeaderMap) -> bool {
    super::header_media_type(headers, header::CONTENT_TYPE)
        .is_some_and(|media_type| media_type.eq_ignore_ascii_case("multipart/form-data"))
}

fn is_upload_field(name: &str) -> bool {
    matches!(
        name,
        "file" | "files" | "attachment" | "attachments" | "photo" | "photos"
    )
}

fn canonical_photo_mime(
    declared_mime: Option<&str>,
    content: &[u8],
) -> Result<&'static str, SessionMessageRequestError> {
    let sniffed = sniff_photo_mime(content).ok_or_else(|| {
        SessionMessageRequestError::bad_request("only jpeg, png, and webp photos are supported")
    })?;
    let dimensions = photo_dimensions(sniffed, content).ok_or_else(|| {
        SessionMessageRequestError::bad_request("photo dimensions could not be verified")
    })?;
    validate_photo_dimensions(dimensions)?;
    if let Some(declared_mime) = declared_mime
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let lower = declared_mime.to_ascii_lowercase();
        if !lower.starts_with("image/") {
            return Err(SessionMessageRequestError::bad_request(
                "only photo uploads are supported",
            ));
        }
        let Some(normalized) = normalize_contact_session_photo_mime(&lower) else {
            return Err(SessionMessageRequestError::bad_request(
                "only jpeg, png, and webp photos are supported",
            ));
        };
        if normalized != sniffed {
            return Err(SessionMessageRequestError::bad_request(
                "photo MIME type does not match content",
            ));
        }
    }
    Ok(sniffed)
}

fn validated_upload_name(file_name: Option<&str>) -> Result<String, SessionMessageRequestError> {
    let candidate = file_name
        .and_then(|name| {
            name.replace('\\', "/")
                .rsplit('/')
                .next()
                .map(str::trim)
                .map(str::to_string)
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "photo".to_string());
    if candidate.len() > MAX_CONTACT_SESSION_ATTACHMENT_NAME_BYTES
        || candidate.chars().any(char::is_control)
    {
        return Err(SessionMessageRequestError::bad_request(
            "photo file name is invalid",
        ));
    }
    Ok(candidate)
}

fn sniff_photo_mime(content: &[u8]) -> Option<&'static str> {
    if content.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if content.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if content.len() >= 12 && &content[0..4] == b"RIFF" && &content[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

fn photo_dimensions(mime_type: &str, content: &[u8]) -> Option<(u32, u32)> {
    match mime_type {
        "image/jpeg" => jpeg_dimensions(content),
        "image/png" => png_dimensions(content),
        "image/webp" => webp_dimensions(content),
        _ => None,
    }
}

fn validate_photo_dimensions(dimensions: (u32, u32)) -> Result<(), SessionMessageRequestError> {
    let (width, height) = dimensions;
    if width == 0 || height == 0 {
        return Err(SessionMessageRequestError::bad_request(
            "photo dimensions are invalid",
        ));
    }
    if width > MAX_SESSION_PHOTO_DIMENSION || height > MAX_SESSION_PHOTO_DIMENSION {
        return Err(SessionMessageRequestError::bad_request(
            "photo dimensions are too large",
        ));
    }
    if u64::from(width) * u64::from(height) > MAX_SESSION_PHOTO_PIXELS {
        return Err(SessionMessageRequestError::bad_request(
            "photo pixel count is too large",
        ));
    }
    Ok(())
}

fn png_dimensions(content: &[u8]) -> Option<(u32, u32)> {
    if content.len() < 33 || !content.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }

    let mut index = 8;
    let mut dimensions = None;
    let mut saw_idat = false;
    while index + 12 <= content.len() {
        let chunk_len = u32::from_be_bytes(content[index..index + 4].try_into().ok()?) as usize;
        let chunk_type = &content[index + 4..index + 8];
        let data_start = index + 8;
        let crc_start = data_start.checked_add(chunk_len)?;
        let next = crc_start.checked_add(4)?;
        if next > content.len() {
            return None;
        }

        match chunk_type {
            b"IHDR" => {
                if index != 8 || chunk_len != 13 {
                    return None;
                }
                let width =
                    u32::from_be_bytes(content[data_start..data_start + 4].try_into().ok()?);
                let height =
                    u32::from_be_bytes(content[data_start + 4..data_start + 8].try_into().ok()?);
                dimensions = Some((width, height));
            }
            b"IDAT" => {
                dimensions?;
                saw_idat = true;
            }
            b"IEND" => {
                if chunk_len != 0 {
                    return None;
                }
                if !saw_idat {
                    return None;
                }
                return dimensions;
            }
            _ => {}
        }

        index = next;
    }
    None
}

fn jpeg_dimensions(content: &[u8]) -> Option<(u32, u32)> {
    if !content.starts_with(&[0xff, 0xd8]) || !content.ends_with(&[0xff, 0xd9]) {
        return None;
    }
    let mut index = 2;
    while index + 3 < content.len() {
        while index < content.len() && content[index] == 0xff {
            index += 1;
        }
        if index >= content.len() {
            return None;
        }
        let marker = content[index];
        index += 1;
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        if index + 2 > content.len() {
            return None;
        }
        let segment_len = usize::from(u16::from_be_bytes(
            content[index..index + 2].try_into().ok()?,
        ));
        if segment_len < 2 || index + segment_len > content.len() {
            return None;
        }
        if is_jpeg_start_of_frame(marker) {
            if segment_len < 7 {
                return None;
            }
            let height = u32::from(u16::from_be_bytes(
                content[index + 3..index + 5].try_into().ok()?,
            ));
            let width = u32::from(u16::from_be_bytes(
                content[index + 5..index + 7].try_into().ok()?,
            ));
            return Some((width, height));
        }
        index += segment_len;
    }
    None
}

fn is_jpeg_start_of_frame(marker: u8) -> bool {
    matches!(
        marker,
        0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc5 | 0xc6 | 0xc7 | 0xc9 | 0xca | 0xcb | 0xcd | 0xce | 0xcf
    )
}

fn webp_dimensions(content: &[u8]) -> Option<(u32, u32)> {
    if content.len() < 30 || &content[0..4] != b"RIFF" || &content[8..12] != b"WEBP" {
        return None;
    }
    let riff_len = u32::from_le_bytes(content[4..8].try_into().ok()?) as usize;
    if riff_len.checked_add(8)? != content.len() {
        return None;
    }
    match &content[12..16] {
        b"VP8X" => {
            let width = read_u24_le(&content[24..27])?.checked_add(1)?;
            let height = read_u24_le(&content[27..30])?.checked_add(1)?;
            Some((width, height))
        }
        b"VP8L" => {
            if content[20] != 0x2f {
                return None;
            }
            let bits = u32::from_le_bytes(content[21..25].try_into().ok()?);
            let width = (bits & 0x3fff).checked_add(1)?;
            let height = ((bits >> 14) & 0x3fff).checked_add(1)?;
            Some((width, height))
        }
        b"VP8 " => {
            if &content[23..26] != b"\x9d\x01\x2a" {
                return None;
            }
            let width = u32::from(u16::from_le_bytes(content[26..28].try_into().ok()?) & 0x3fff);
            let height = u32::from(u16::from_le_bytes(content[28..30].try_into().ok()?) & 0x3fff);
            Some((width, height))
        }
        _ => None,
    }
}

fn read_u24_le(bytes: &[u8]) -> Option<u32> {
    let bytes: [u8; 3] = bytes.try_into().ok()?;
    Some(u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16))
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use moa_core::{SessionId, TenantId};

    use super::*;

    #[test]
    fn session_message_request_uses_path_session_id() {
        // Pins: browser clients send the session once in the path; conflicting body values cannot retarget the message.
        let path_session_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("path session id should parse");
        let body = Bytes::from_static(
            br#"{
                "tenant_id":"22222222-2222-2222-2222-222222222222",
                "session_id":"33333333-3333-3333-3333-333333333333",
                "contact_token":"token",
                "user_message":"hello"
            }"#,
        );

        let request = contact_session_message_request(path_session_id, &body)
            .expect("message request should decode");

        assert_eq!(request.session_id, SessionId(path_session_id));
        assert_eq!(
            request.tenant_id,
            TenantId::from(
                Uuid::parse_str("22222222-2222-2222-2222-222222222222")
                    .expect("tenant id should parse")
            )
        );
        assert_eq!(request.contact_token, "token");
        assert_eq!(request.user_message, "hello");
    }

    #[test]
    fn session_message_request_rejects_json_attachment_refs() {
        // Pins: public clients must upload attachments as multipart so the edge can validate bytes.
        let path_session_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("path session id should parse");
        let body = Bytes::from_static(
            br#"{
                "tenant_id":"22222222-2222-2222-2222-222222222222",
                "contact_token":"token",
                "attachments":[{
                    "name":"receipt.png",
                    "mime_type":"image/png",
                    "url":"/v1/sessions/11111111-1111-1111-1111-111111111111/attachments/33333333-3333-3333-3333-333333333333",
                    "path":null,
                    "size_bytes":128
                }]
            }"#,
        );

        let error = contact_session_message_request(path_session_id, &body)
            .expect_err("json attachment refs should be rejected");

        assert_eq!(
            error,
            "session message attachments must be uploaded as multipart"
        );
    }

    #[test]
    fn session_message_request_rejects_empty_body() {
        // Pins: a contact message must contain either text or at least one attachment ref.
        let path_session_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("path session id should parse");
        let body = Bytes::from_static(
            br#"{
                "tenant_id":"22222222-2222-2222-2222-222222222222",
                "contact_token":"token"
            }"#,
        );

        let error = contact_session_message_request(path_session_id, &body)
            .expect_err("empty message should be rejected");

        assert_eq!(
            error,
            "contact session message requires text or an attachment"
        );
    }

    #[test]
    fn session_message_request_rejects_oversized_text() {
        // Pins: public JSON contact messages cannot force huge text into Restate/session history.
        let path_session_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("path session id should parse");
        let body = Bytes::from(
            serde_json::json!({
                "tenant_id": "22222222-2222-2222-2222-222222222222",
                "contact_token": "token",
                "user_message": "x".repeat(moa_core::MAX_CONTACT_SESSION_MESSAGE_TEXT_BYTES + 1),
            })
            .to_string(),
        );

        let error = contact_session_message_request(path_session_id, &body)
            .expect_err("oversized message should be rejected");

        assert_eq!(error, "session message text is too long");
    }

    #[test]
    fn canonical_photo_mime_requires_supported_image_content() {
        // Pins: upload admission trusts content sniffing over caller-declared MIME type.
        let png = png_with_dimensions(640, 480);
        assert_eq!(
            canonical_photo_mime(Some("image/png"), &png).expect("valid png should be accepted"),
            "image/png"
        );
        assert_eq!(
            canonical_photo_mime(Some("image/jpeg"), &png)
                .expect_err("declared MIME mismatch should be rejected")
                .message,
            "photo MIME type does not match content"
        );
        assert_eq!(
            canonical_photo_mime(Some("application/pdf"), b"%PDF")
                .expect_err("non-photo bytes should be rejected")
                .message,
            "only jpeg, png, and webp photos are supported"
        );
        assert_eq!(
            canonical_photo_mime(Some("image/gif"), &png)
                .expect_err("unsupported declared image type should be rejected")
                .message,
            "only jpeg, png, and webp photos are supported"
        );
    }

    #[test]
    fn canonical_photo_mime_rejects_decompression_bomb_dimensions() {
        // Pins: compressed image bytes must declare bounded dimensions before storage.
        let huge_png = png_with_dimensions(40_000, 40_000);

        let error = canonical_photo_mime(Some("image/png"), &huge_png)
            .expect_err("huge image dimensions should be rejected");

        assert_eq!(error.message, "photo dimensions are too large");
    }

    #[test]
    fn canonical_photo_mime_rejects_header_only_png() {
        // Pins: upload admission requires a minimally structured image container, not only magic bytes.
        let mut header_only_png = Vec::from(&b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"[..]);
        header_only_png.extend_from_slice(&640_u32.to_be_bytes());
        header_only_png.extend_from_slice(&480_u32.to_be_bytes());
        header_only_png.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);

        let error = canonical_photo_mime(Some("image/png"), &header_only_png)
            .expect_err("header-only png should be rejected");

        assert_eq!(error.message, "photo dimensions could not be verified");
    }

    #[test]
    fn validated_upload_name_rejects_control_characters() {
        // Pins: caller-supplied display names cannot carry control bytes into stored attachment metadata.
        let error = validated_upload_name(Some("invoice\n.png"))
            .expect_err("control characters should be rejected");

        assert_eq!(error.message, "photo file name is invalid");
    }

    fn png_with_dimensions(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::from(&b"\x89PNG\r\n\x1a\n"[..]);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        append_png_chunk(&mut bytes, b"IHDR", &ihdr);
        append_png_chunk(
            &mut bytes,
            b"IDAT",
            &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
        );
        append_png_chunk(&mut bytes, b"IEND", &[]);
        bytes
    }

    fn append_png_chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
    }
}
