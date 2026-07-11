//! Contact repository operations grouped by persisted aggregate.

mod channel_accounts;
mod contacts;
mod row_mapping;
mod token_grants;
mod verification;

pub use channel_accounts::{ResolvedSessionChannel, resolve_contact_session_channel};
pub use contacts::{
    issue_contact, load_contact_ref, promoted_from_contact, resolve_verified_contact_ids,
};
pub use token_grants::{create_contact_token_grant, ensure_contact_token_grant_active};
pub use verification::complete_contact_verification;

pub(crate) use verification::{
    CreatedContactVerificationChallenge, consume_contact_verification_challenge,
    create_contact_verification_challenge,
};
