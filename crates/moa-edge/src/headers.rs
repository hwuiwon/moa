//! Header contract between `moa-edge` and `moa-orchestrator`.
//!
//! These headers are set by `moa-edge` on forwarded requests and are not
//! externally trustworthy. The proxy strips inbound `X-Moa-*` values before
//! injecting its resolved identity headers.

/// Principal type header.
pub const H_IDENTITY_TYPE: &str = "x-moa-identity-type";
/// Principal UUID header.
pub const H_IDENTITY_ID: &str = "x-moa-identity-id";
/// Tenant UUID header.
pub const H_TENANT_ID: &str = "x-moa-tenant-id";
/// API key UUID header.
pub const H_API_KEY_ID: &str = "x-moa-api-key-id";
/// Delegating user UUID header.
pub const H_ACTING_ON_BEHALF_OF: &str = "x-moa-acting-on-behalf-of";

/// Returns true when a header name belongs to the MOA identity namespace.
///
/// Matched case-insensitively against the `x-moa-` prefix (which covers every
/// identity header written by `moa-edge`) so callers need not allocate a
/// lowercased copy per header.
#[must_use]
pub fn is_moa_header(name: &str) -> bool {
    const PREFIX: &[u8] = b"x-moa-";
    let bytes = name.as_bytes();
    bytes.len() >= PREFIX.len() && bytes[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
}
