//! Adversarial out-of-line tests for Ed25519 audit signing helpers.

mod support;

use moa_lineage_audit::{AuditError, SigningKey};
use serde::Deserialize;
use support::fixture_json;

#[derive(Debug, Deserialize)]
struct Ed25519Vectors {
    cases: Vec<Ed25519Case>,
}

#[derive(Debug, Deserialize)]
struct Ed25519Case {
    name: String,
    secret_key_hex: String,
    public_key_hex: String,
    message_hex: String,
    signature_hex: String,
}

#[test]
fn ed25519_sign_with_rfc8032_test_keypair_produces_expected_signature() {
    let case = vector_case("test_1_empty_message");
    let key = signing_key(&case);
    let message = hex::decode(&case.message_hex).expect("message should decode");
    let expected_signature = hex::decode(&case.signature_hex).expect("signature should decode");
    let expected_public_key = hex::decode(&case.public_key_hex).expect("public key should decode");

    assert_eq!(
        key.verifying_key_bytes().as_slice(),
        expected_public_key.as_slice()
    );
    assert_eq!(key.sign_message(&message), expected_signature);
}

#[test]
fn ed25519_verify_succeeds_for_valid_signature_and_keypair_pair() {
    let case = vector_case("test_2_one_byte_message");
    let key = signing_key(&case);
    let message = hex::decode(&case.message_hex).expect("message should decode");
    let signature = key.sign_message(&message);

    key.verify_message(&message, &signature)
        .expect("valid signature should verify");
}

#[test]
fn ed25519_verify_fails_for_signature_under_different_keypair() {
    let signing_case = vector_case("test_1_empty_message");
    let verifying_case = vector_case("test_2_one_byte_message");
    let signer = signing_key(&signing_case);
    let verifier = signing_key(&verifying_case);
    let message = hex::decode(&signing_case.message_hex).expect("message should decode");
    let signature = signer.sign_message(&message);

    let error = verifier
        .verify_message(&message, &signature)
        .expect_err("signature should not verify under a different public key");

    assert!(matches!(error, AuditError::Signature));
}

/// Tampering: flips the low bit of signature byte 0 before verification.
#[test]
fn ed25519_verify_fails_for_corrupted_signature_with_one_flipped_bit() {
    let case = vector_case("test_2_one_byte_message");
    let key = signing_key(&case);
    let message = hex::decode(&case.message_hex).expect("message should decode");
    let mut signature = key.sign_message(&message);
    signature[0] ^= 1;

    let error = key
        .verify_message(&message, &signature)
        .expect_err("corrupted signature should not verify");

    assert!(matches!(error, AuditError::Signature));
}

/// Tampering: flips the low bit of message byte 0 before verification.
#[test]
fn ed25519_verify_fails_for_message_with_one_flipped_bit() {
    let case = vector_case("test_2_one_byte_message");
    let key = signing_key(&case);
    let mut message = hex::decode(&case.message_hex).expect("message should decode");
    let signature = key.sign_message(&message);
    message[0] ^= 1;

    let error = key
        .verify_message(&message, &signature)
        .expect_err("mutated message should not verify with the original signature");

    assert!(matches!(error, AuditError::Signature));
}

fn vector_case(case_name: &str) -> Ed25519Case {
    let vectors: Ed25519Vectors =
        serde_json::from_value(fixture_json("ed25519_rfc8032_vectors.json"))
            .expect("Ed25519 vector fixture should parse");
    vectors
        .cases
        .into_iter()
        .find(|case| case.name == case_name)
        .unwrap_or_else(|| panic!("missing Ed25519 vector case {case_name}"))
}

fn signing_key(case: &Ed25519Case) -> SigningKey {
    let seed = hex::decode(&case.secret_key_hex).expect("secret key should decode");
    let seed: [u8; 32] = seed
        .try_into()
        .expect("RFC 8032 Ed25519 secret key should be 32 bytes");
    SigningKey::from_seed(case.name.clone(), seed)
}
