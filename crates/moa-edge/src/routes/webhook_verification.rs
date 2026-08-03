//! Edge-local webhook signature verification helpers.

use axum::http::{HeaderMap, StatusCode};
use base64::{Engine as _, engine::general_purpose};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use super::KnowledgeWebhookEdgeConfig;

pub(super) fn verify_knowledge_webhook_at_edge(
    provider: &str,
    headers: &HeaderMap,
    body: &[u8],
    config: &KnowledgeWebhookEdgeConfig,
) -> Result<(), (StatusCode, &'static str)> {
    match provider {
        "nango" => {
            let signing_key = config
                .nango_signing_key
                .as_deref()
                .ok_or((StatusCode::UNAUTHORIZED, "missing webhook verifier"))?;
            verify_hmac_header(headers, body, signing_key, "x-nango-hmac-sha256")
        }
        "merge" => {
            let signing_key = config
                .merge_signature_key
                .as_deref()
                .ok_or((StatusCode::UNAUTHORIZED, "missing webhook verifier"))?;
            verify_hmac_header(headers, body, signing_key, "x-merge-webhook-signature")
        }
        "llamaparse" => verify_parser_webhook_at_edge(
            "llamaparse",
            headers,
            body,
            config.llamaparse_signing_key.as_deref(),
            config.llamaparse_custom_header.as_ref(),
        ),
        "reducto" => verify_parser_webhook_at_edge(
            "reducto",
            headers,
            body,
            config.reducto_signing_key.as_deref(),
            config.reducto_custom_header.as_ref(),
        ),
        _ => Err((
            StatusCode::BAD_REQUEST,
            "unknown knowledge webhook provider",
        )),
    }
}

fn verify_parser_webhook_at_edge(
    parser: &str,
    headers: &HeaderMap,
    body: &[u8],
    signing_key: Option<&str>,
    custom_header: Option<&(String, String)>,
) -> Result<(), (StatusCode, &'static str)> {
    if let Some((name, expected)) = custom_header
        && !verify_custom_header(headers, name, expected)
    {
        return Err((StatusCode::UNAUTHORIZED, "invalid webhook header"));
    }
    let signing_key = signing_key.ok_or((StatusCode::UNAUTHORIZED, "missing webhook verifier"))?;
    if super::webhook_header_value(headers, &["svix-signature", "x-svix-signature"]).is_some() {
        return verify_svix_signature_at_edge(headers, body, signing_key);
    }
    let parser_header = format!("x-{parser}-webhook-signature");
    verify_hmac_header_candidates(
        headers,
        body,
        signing_key,
        &[parser_header.as_str(), "x-moa-knowledge-webhook-signature"],
    )
}

fn verify_hmac_header(
    headers: &HeaderMap,
    body: &[u8],
    signing_key: &str,
    header_name: &str,
) -> Result<(), (StatusCode, &'static str)> {
    verify_hmac_header_candidates(headers, body, signing_key, &[header_name])
}

fn verify_hmac_header_candidates(
    headers: &HeaderMap,
    body: &[u8],
    signing_key: &str,
    header_names: &[&str],
) -> Result<(), (StatusCode, &'static str)> {
    let signature = super::webhook_header_value(headers, header_names)
        .ok_or((StatusCode::UNAUTHORIZED, "missing webhook signature"))?;
    let signature = decode_webhook_signature(&signature)
        .ok_or((StatusCode::UNAUTHORIZED, "invalid webhook signature"))?;
    verify_hmac_signature(signing_key.as_bytes(), body, &signature)
}

fn verify_svix_signature_at_edge(
    headers: &HeaderMap,
    body: &[u8],
    signing_key: &str,
) -> Result<(), (StatusCode, &'static str)> {
    let message_id = super::webhook_header_value(headers, &["svix-id", "x-svix-id"])
        .ok_or((StatusCode::UNAUTHORIZED, "missing webhook signature"))?;
    let timestamp = super::webhook_header_value(headers, &["svix-timestamp", "x-svix-timestamp"])
        .ok_or((StatusCode::UNAUTHORIZED, "missing webhook signature"))?;
    let timestamp = timestamp
        .parse::<i64>()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid webhook signature"))?;
    let now = chrono::Utc::now().timestamp();
    if (now - timestamp).abs() > 300 {
        return Err((StatusCode::UNAUTHORIZED, "stale webhook signature"));
    }
    let signature = super::webhook_header_value(headers, &["svix-signature", "x-svix-signature"])
        .ok_or((StatusCode::UNAUTHORIZED, "missing webhook signature"))?;
    let key = svix_signing_key(signing_key)
        .ok_or((StatusCode::UNAUTHORIZED, "invalid webhook verifier"))?;
    let body = std::str::from_utf8(body)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid webhook signature"))?;
    let signed_payload = format!("{message_id}.{timestamp}.{body}");
    for candidate in signature.split_whitespace() {
        if let Some(encoded) = candidate.strip_prefix("v1,")
            && let Some(signature) = decode_base64_signature(encoded)
            && verify_hmac_signature(&key, signed_payload.as_bytes(), &signature).is_ok()
        {
            return Ok(());
        }
    }
    Err((StatusCode::UNAUTHORIZED, "invalid webhook signature"))
}

fn verify_hmac_signature(
    signing_key: &[u8],
    body: &[u8],
    signature: &[u8],
) -> Result<(), (StatusCode, &'static str)> {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(signing_key) else {
        return Err((StatusCode::UNAUTHORIZED, "invalid webhook verifier"));
    };
    mac.update(body);
    mac.verify_slice(signature)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid webhook signature"))
}

fn verify_custom_header(headers: &HeaderMap, name: &str, expected: &str) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|actual| actual.as_bytes().ct_eq(expected.as_bytes()).into())
}

fn decode_webhook_signature(value: &str) -> Option<Vec<u8>> {
    let value = value.trim().trim_start_matches("sha256=");
    if let Ok(decoded) = hex::decode(value)
        && decoded.len() == 32
    {
        return Some(decoded);
    }
    decode_base64_signature(value)
}

fn decode_base64_signature(value: &str) -> Option<Vec<u8>> {
    general_purpose::STANDARD
        .decode(value.trim())
        .or_else(|_| general_purpose::URL_SAFE.decode(value.trim()))
        .or_else(|_| general_purpose::URL_SAFE_NO_PAD.decode(value.trim()))
        .ok()
}

fn svix_signing_key(signing_key: &str) -> Option<Vec<u8>> {
    let Some(encoded) = signing_key.trim().strip_prefix("whsec_") else {
        return Some(signing_key.as_bytes().to_vec());
    };
    decode_base64_signature(encoded)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;
    use chrono::Utc;

    use super::*;

    /// Computes the hex-encoded HMAC-SHA256 of `body` under `key`, mirroring how
    /// upstream webhook providers sign payloads so the verifier's accept path is
    /// exercised with a genuinely valid signature rather than a precomputed constant.
    fn hmac_sha256_hex(key: &[u8], body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn verify_hmac_header_accepts_valid_signature_and_rejects_tampering() {
        // Pins: the webhook HMAC header verifier (the 401 ingress gate) accepts a
        // correctly signed body, rejects the same signature over a tampered body, and
        // rejects a request with no signature header.
        let key = "edge-webhook-secret";
        let body = br#"{"event":"document.processed"}"#;
        let header = "x-moa-knowledge-webhook-signature";

        let mut headers = HeaderMap::new();
        headers.insert(
            header,
            hmac_sha256_hex(key.as_bytes(), body)
                .parse()
                .expect("hex signature is a valid header value"),
        );

        assert!(verify_hmac_header(&headers, body, key, header).is_ok());

        let tampered = br#"{"event":"document.deleted!"}"#;
        assert_eq!(
            verify_hmac_header(&headers, tampered, key, header),
            Err((StatusCode::UNAUTHORIZED, "invalid webhook signature"))
        );

        assert_eq!(
            verify_hmac_header(&HeaderMap::new(), body, key, header),
            Err((StatusCode::UNAUTHORIZED, "missing webhook signature"))
        );
    }

    #[test]
    fn verify_hmac_signature_distinguishes_matching_from_forged_signatures() {
        // Pins: the raw HMAC-SHA256 comparison accepts the exact signature, rejects a
        // signature computed over different bytes, and rejects an empty signature.
        let key = b"shared-secret";
        let body = b"payload-bytes";

        let valid = {
            let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("valid key");
            mac.update(body);
            mac.finalize().into_bytes()
        };
        assert!(verify_hmac_signature(key, body, valid.as_slice()).is_ok());

        let forged = {
            let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("valid key");
            mac.update(b"other-bytes");
            mac.finalize().into_bytes()
        };
        assert_eq!(
            verify_hmac_signature(key, body, forged.as_slice()),
            Err((StatusCode::UNAUTHORIZED, "invalid webhook signature"))
        );

        assert_eq!(
            verify_hmac_signature(key, body, &[]),
            Err((StatusCode::UNAUTHORIZED, "invalid webhook signature"))
        );
    }

    #[test]
    fn verify_svix_signature_accepts_valid_and_rejects_tampered_or_missing() {
        // Pins: the Svix-style edge verifier accepts a `v1,` signature over the
        // `id.timestamp.body` payload, rejects a tampered body, and rejects a request
        // whose signature header is absent (id + timestamp present).
        let key = "svix-shared-secret";
        let message_id = "msg_2abc";
        let timestamp = Utc::now().timestamp();
        let body = r#"{"type":"connection.created"}"#;
        let signed_payload = format!("{message_id}.{timestamp}.{body}");

        let signature = {
            let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("valid key");
            mac.update(signed_payload.as_bytes());
            general_purpose::STANDARD.encode(mac.finalize().into_bytes())
        };

        let mut headers = HeaderMap::new();
        headers.insert("svix-id", message_id.parse().expect("valid header value"));
        headers.insert(
            "svix-timestamp",
            timestamp.to_string().parse().expect("valid header value"),
        );
        headers.insert(
            "svix-signature",
            format!("v1,{signature}")
                .parse()
                .expect("valid header value"),
        );

        assert!(verify_svix_signature_at_edge(&headers, body.as_bytes(), key).is_ok());

        assert_eq!(
            verify_svix_signature_at_edge(&headers, b"{}", key),
            Err((StatusCode::UNAUTHORIZED, "invalid webhook signature"))
        );

        let mut without_sig = HeaderMap::new();
        without_sig.insert("svix-id", message_id.parse().expect("valid header value"));
        without_sig.insert(
            "svix-timestamp",
            timestamp.to_string().parse().expect("valid header value"),
        );
        assert_eq!(
            verify_svix_signature_at_edge(&without_sig, body.as_bytes(), key),
            Err((StatusCode::UNAUTHORIZED, "missing webhook signature"))
        );
    }
}
