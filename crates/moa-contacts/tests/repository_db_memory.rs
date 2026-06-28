//! Postgres repository coverage for contact issuance, OTP verification, token
//! grants, channel resolution, and tenant isolation.
//!
//! `start_contact_verification` requires a configured messaging provider, so the
//! verification round-trips seed the challenge row directly (the only piece that
//! depends on outbound delivery) and then drive the real
//! `complete_contact_verification` production path.

use chrono::{Duration, Utc};
use moa_contacts::domain::hash_verification_code;
use moa_contacts::repository::{
    complete_contact_verification, create_contact_token_grant, ensure_contact_token_grant_active,
    issue_contact, load_contact_ref, resolve_contact_session_channel, resolve_verified_contact_ids,
};
use moa_core::{
    Channel, ChannelAccountId, ChannelRef, ContactId, ContactPointId, ContactPointInput,
    ContactPointKind, ContactTokenClaims, ContactTokenIssueRequest, ContactVerificationChallengeId,
    ContactVerificationState, StoragePartitionId, TenantId,
};
use moa_test_support::postgres;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// Fixed 32-byte (64 hex char) contact-point hash key used across the suite.
const KEY_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn contacts_repository_issue_verify_and_grant_round_trip_db_memory() {
    // Pins: issue -> token-grant -> OTP verify -> verified-contact resolution persist, and the pre-verification grant is revoked on success.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated contacts DB");
    let pool = db.store().pool();
    let tenant = TenantId::from(Uuid::now_v7());

    let (contact, points) = issue_contact(
        pool.clone(),
        KEY_HEX,
        tenant,
        issue_request(tenant, vec![email_point("User@Example.com")]),
    )
    .await
    .expect("issue contact with one unverified email point");
    assert_eq!(contact.state, ContactVerificationState::Unverified);
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].kind, ContactPointKind::Email);
    assert!(!points[0].verified);

    // An issued token grant round-trips and reads back as active.
    let claims = token_claims(
        tenant,
        contact.contact_id,
        ContactVerificationState::Unverified,
    );
    create_contact_token_grant(
        pool.clone(),
        &claims,
        contact.contact_id,
        Utc::now() + Duration::hours(1),
        "identity",
        None,
    )
    .await
    .expect("persist contact token grant");
    ensure_contact_token_grant_active(pool, &claims, contact.contact_id)
        .await
        .expect("grant is active before verification");

    // No verified contact resolves yet.
    assert!(
        resolve_verified_contact_ids(pool, tenant, KEY_HEX, &[email_point("user@example.com")])
            .await
            .expect("resolve before verification")
            .is_empty()
    );

    // Complete OTP verification through the real production path.
    let challenge_id =
        seed_challenge(pool, tenant, contact.contact_id, points[0].id, "424242").await;
    let verified = complete_contact_verification(
        pool.clone(),
        tenant,
        contact.contact_id,
        challenge_id,
        "424242".to_string(),
    )
    .await
    .expect("verify with the matching OTP code");
    assert_eq!(verified.state, ContactVerificationState::Verified);

    // The verified contact now resolves by hashed contact point (case-insensitive).
    assert_eq!(
        resolve_verified_contact_ids(pool, tenant, KEY_HEX, &[email_point("user@example.com")])
            .await
            .expect("resolve after verification"),
        vec![contact.contact_id]
    );

    // Verification revokes the pre-verification grant.
    let revoked = ensure_contact_token_grant_active(pool, &claims, contact.contact_id)
        .await
        .expect_err("pre-verification grant must be revoked after verification");
    assert_eq!(revoked.terminal_code(), Some(401));

    // Reloading the projection reflects the verified state.
    assert_eq!(
        load_contact_ref(pool.clone(), tenant, contact.contact_id)
            .await
            .expect("reload verified contact")
            .state,
        ContactVerificationState::Verified
    );
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn contacts_repository_isolates_contacts_across_tenants_db_memory() {
    // Pins: contact reads and verified-point resolution are tenant-scoped; one tenant cannot observe another's rows.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated contacts DB");
    let pool = db.store().pool();
    let tenant_a = TenantId::from(Uuid::now_v7());
    let tenant_b = TenantId::from(Uuid::now_v7());

    let (contact_a, points_a) = issue_contact(
        pool.clone(),
        KEY_HEX,
        tenant_a,
        issue_request(tenant_a, vec![email_point("shared@example.com")]),
    )
    .await
    .expect("issue tenant A contact");
    issue_contact(
        pool.clone(),
        KEY_HEX,
        tenant_b,
        issue_request(tenant_b, vec![email_point("shared@example.com")]),
    )
    .await
    .expect("issue tenant B contact");

    // Tenant B cannot load tenant A's contact, but tenant A can.
    let cross = load_contact_ref(pool.clone(), tenant_b, contact_a.contact_id)
        .await
        .expect_err("tenant B must not read tenant A contact");
    assert_eq!(cross.terminal_code(), Some(404));
    load_contact_ref(pool.clone(), tenant_a, contact_a.contact_id)
        .await
        .expect("tenant A reads its own contact");

    // Verify tenant A's email point.
    let challenge_id = seed_challenge(
        pool,
        tenant_a,
        contact_a.contact_id,
        points_a[0].id,
        "100200",
    )
    .await;
    complete_contact_verification(
        pool.clone(),
        tenant_a,
        contact_a.contact_id,
        challenge_id,
        "100200".to_string(),
    )
    .await
    .expect("verify tenant A contact");

    // Tenant A resolves its verified point; tenant B resolves nothing for the same address.
    assert_eq!(
        resolve_verified_contact_ids(
            pool,
            tenant_a,
            KEY_HEX,
            &[email_point("shared@example.com")]
        )
        .await
        .expect("tenant A resolves verified contact"),
        vec![contact_a.contact_id]
    );
    assert!(
        resolve_verified_contact_ids(
            pool,
            tenant_b,
            KEY_HEX,
            &[email_point("shared@example.com")]
        )
        .await
        .expect("tenant B resolves nothing across the tenant boundary")
        .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn contacts_repository_resolves_and_validates_session_channels_db_memory() {
    // Pins: chat routes materialize an idempotent channel account; malformed Slack routes and unknown email accounts are rejected.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated contacts DB");
    let pool = db.store().pool();
    let tenant = TenantId::from(Uuid::now_v7());

    let (contact, _points) = issue_contact(
        pool.clone(),
        KEY_HEX,
        tenant,
        issue_request(tenant, Vec::new()),
    )
    .await
    .expect("issue anonymous contact");
    assert_eq!(contact.state, ContactVerificationState::Anonymous);

    // A chat route materializes a channel account.
    let chat = ChannelRef::Chat {
        conversation_id: "conv-1".to_string(),
        user_id: Some("user-9".to_string()),
        client_session_id: None,
    };
    let resolved = resolve_contact_session_channel(pool, &contact, chat.clone())
        .await
        .expect("resolve chat channel");
    let account = resolved
        .channel_account
        .expect("chat route resolves to a channel account");
    assert_eq!(account.channel, Channel::Chat);

    // Resolving the same external user is idempotent (same account id).
    let again = resolve_contact_session_channel(pool, &contact, chat)
        .await
        .expect("resolve chat channel again");
    assert_eq!(
        again
            .channel_account
            .expect("repeat chat route resolves to an account")
            .channel_account_id,
        account.channel_account_id
    );

    // A Slack route without a user id is a terminal 400.
    let slack = ChannelRef::Slack {
        team_id: Some("T1".to_string()),
        slack_channel_id: Some("C1".to_string()),
        thread_ts: None,
        user_id: None,
    };
    let slack_err = resolve_contact_session_channel(pool, &contact, slack)
        .await
        .expect_err("slack route requires a user_id");
    assert_eq!(slack_err.terminal_code(), Some(400));

    // An unknown email channel account is a terminal 404.
    let email = ChannelRef::Email {
        channel_account_id: ChannelAccountId::new(),
    };
    let email_err = resolve_contact_session_channel(pool, &contact, email)
        .await
        .expect_err("unknown email channel account is not found");
    assert_eq!(email_err.terminal_code(), Some(404));
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn contacts_repository_rejects_invalid_verification_code_db_memory() {
    // Pins: a wrong OTP code is a terminal 403 and leaves the contact unverified.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated contacts DB");
    let pool = db.store().pool();
    let tenant = TenantId::from(Uuid::now_v7());

    let (contact, points) = issue_contact(
        pool.clone(),
        KEY_HEX,
        tenant,
        issue_request(tenant, vec![email_point("user@example.com")]),
    )
    .await
    .expect("issue contact");
    let challenge_id =
        seed_challenge(pool, tenant, contact.contact_id, points[0].id, "111111").await;

    let err = complete_contact_verification(
        pool.clone(),
        tenant,
        contact.contact_id,
        challenge_id,
        "999999".to_string(),
    )
    .await
    .expect_err("a mismatched OTP code must reject");
    assert_eq!(err.terminal_code(), Some(403));

    assert_eq!(
        load_contact_ref(pool.clone(), tenant, contact.contact_id)
            .await
            .expect("reload contact after failed verification")
            .state,
        ContactVerificationState::Unverified
    );
}

fn issue_request(
    tenant: TenantId,
    contact_points: Vec<ContactPointInput>,
) -> ContactTokenIssueRequest {
    ContactTokenIssueRequest {
        tenant_id: tenant,
        contact_points,
        display_name: None,
        profile: json!({}),
        metadata: json!({}),
        requested_scopes: Vec::new(),
        permissions: json!({}),
        agent_ids: Vec::new(),
    }
}

fn email_point(value: &str) -> ContactPointInput {
    ContactPointInput {
        kind: ContactPointKind::Email,
        value: value.to_string(),
        display_value: Some(value.to_string()),
    }
}

fn token_claims(
    tenant: TenantId,
    contact_id: ContactId,
    state: ContactVerificationState,
) -> ContactTokenClaims {
    ContactTokenClaims {
        iss: "moa-test".to_string(),
        aud: "moa-contact".to_string(),
        sub: contact_id.to_string(),
        exp: 0,
        iat: 0,
        nbf: 0,
        jti: Uuid::now_v7().to_string(),
        tenant_id: tenant,
        state,
        scopes: vec!["agent:session:create".to_string()],
        permissions: json!({}),
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
        linked_contact_ids: Vec::new(),
    }
}

/// Seeds a pending verification challenge for `code` and returns its id.
///
/// This is the only delivery-dependent step that cannot run hermetically, so the
/// row is inserted directly while `complete_contact_verification` exercises the
/// real verification path.
async fn seed_challenge(
    pool: &PgPool,
    tenant: TenantId,
    contact_id: ContactId,
    contact_point_id: ContactPointId,
    code: &str,
) -> ContactVerificationChallengeId {
    let challenge_id = ContactVerificationChallengeId::new();
    sqlx::query(
        r#"
        INSERT INTO contact_verification_challenges
            (id, contact_id, contact_point_id, tenant_id, storage_partition_id, code_hash, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(challenge_id.0)
    .bind(contact_id.0)
    .bind(contact_point_id.0)
    .bind(tenant.0)
    .bind(StoragePartitionId::for_tenant(tenant).as_str())
    .bind(hash_verification_code(challenge_id, code))
    .bind(Utc::now() + Duration::hours(1))
    .execute(pool)
    .await
    .expect("seed verification challenge");
    challenge_id
}
