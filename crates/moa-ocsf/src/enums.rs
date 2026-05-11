//! OCSF v1.3 enum values used by MOA security events.

/// OCSF class UIDs.
pub mod class_uid {
    /// Account Change.
    pub const ACCOUNT_CHANGE: i32 = 3001;
    /// Authentication.
    pub const AUTHENTICATION: i32 = 3002;
    /// Authorize Session.
    pub const AUTHORIZATION: i32 = 3003;
    /// Entity Management.
    pub const ENTITY_MANAGEMENT: i32 = 3004;
}

/// OCSF category UIDs.
pub mod category_uid {
    /// Identity & Access Management.
    pub const IAM: i32 = 3;
}

/// OCSF severity IDs.
pub mod severity_id {
    /// Informational.
    pub const INFORMATIONAL: i32 = 1;
    /// Low.
    pub const LOW: i32 = 2;
    /// Medium.
    pub const MEDIUM: i32 = 3;
    /// High.
    pub const HIGH: i32 = 4;
    /// Critical.
    pub const CRITICAL: i32 = 5;
    /// Fatal.
    pub const FATAL: i32 = 6;
}

/// Authentication activity IDs.
pub mod authn_activity {
    /// Logon.
    pub const LOGON: i32 = 1;
    /// Credential Validation.
    pub const CREDENTIAL_VALIDATION: i32 = 5;
}

/// Authentication status IDs.
pub mod authn_status {
    /// Success.
    pub const SUCCESS: i32 = 1;
    /// Failure.
    pub const FAILURE: i32 = 2;
}

/// Authorization activity IDs.
pub mod authz_activity {
    /// Grant Privileges.
    pub const GRANT_PRIVILEGES: i32 = 1;
    /// Revoke Privileges.
    pub const REVOKE_PRIVILEGES: i32 = 2;
    /// Other authorization activity.
    pub const OTHER: i32 = 99;
}

/// Authorization status IDs.
pub mod authz_status {
    /// Allowed.
    pub const ALLOWED: i32 = 1;
    /// Denied.
    pub const DENIED: i32 = 2;
}

/// Entity Management activity IDs.
pub mod entity_activity {
    /// Create.
    pub const CREATE: i32 = 1;
    /// Read.
    pub const READ: i32 = 2;
    /// Update.
    pub const UPDATE: i32 = 3;
    /// Delete.
    pub const DELETE: i32 = 4;
    /// Other entity activity.
    pub const OTHER: i32 = 99;
}

/// Account Change activity IDs.
pub mod account_activity {
    /// Create.
    pub const CREATE: i32 = 1;
    /// Enable.
    pub const ENABLE: i32 = 2;
    /// Disable.
    pub const DISABLE: i32 = 3;
    /// Delete.
    pub const DELETE: i32 = 4;
    /// Other account change.
    pub const OTHER: i32 = 99;
}
