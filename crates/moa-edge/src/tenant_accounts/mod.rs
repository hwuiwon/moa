//! Tenant-account application and persistence boundaries.

use chrono::{DateTime, Utc};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub(crate) mod application;
pub(crate) mod repository;

/// Public tenant signup request.
#[derive(Debug, Deserialize)]
pub(crate) struct TenantSignupRequest {
    pub(crate) name: String,
    pub(crate) slug: String,
    pub(crate) admin_email: String,
    pub(crate) admin_password: String,
    pub(crate) admin_display_name: Option<String>,
    pub(crate) admin_given_name: Option<String>,
    pub(crate) admin_family_name: Option<String>,
    pub(crate) settings: Option<Value>,
}

/// Tenant settings mutation.
#[derive(Debug, Deserialize)]
pub(crate) struct PatchTenantRequest {
    pub(crate) name: Option<String>,
    pub(crate) settings: Option<Value>,
}

/// Tenant-admin user creation request.
#[derive(Debug, Deserialize)]
pub(crate) struct CreateTenantUserRequest {
    pub(crate) email: String,
    pub(crate) password: String,
    pub(crate) role: TenantUserRole,
    pub(crate) display_name: Option<String>,
    pub(crate) given_name: Option<String>,
    pub(crate) family_name: Option<String>,
    pub(crate) settings: Option<Value>,
}

/// Tenant-admin invitation request.
#[derive(Debug, Deserialize)]
pub(crate) struct InviteTenantUserRequest {
    pub(crate) email: String,
    pub(crate) role: TenantUserRole,
    pub(crate) display_name: Option<String>,
    pub(crate) given_name: Option<String>,
    pub(crate) family_name: Option<String>,
    pub(crate) settings: Option<Value>,
}

/// Tenant invitation acceptance request.
#[derive(Debug, Deserialize)]
pub(crate) struct AcceptTenantInvitationRequest {
    pub(crate) token: String,
    pub(crate) password: String,
    pub(crate) display_name: Option<String>,
    pub(crate) given_name: Option<String>,
    pub(crate) family_name: Option<String>,
}

/// Tenant user role assignable through tenant-scoped account endpoints.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TenantUserRole {
    Admin,
    Operator,
}

impl TenantUserRole {
    pub(crate) fn relation(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Operator => "operator",
        }
    }

    pub(crate) fn from_relation(relation: &str) -> Option<Self> {
        match relation {
            "admin" => Some(Self::Admin),
            "operator" => Some(Self::Operator),
            _ => None,
        }
    }
}

/// Tenant account response.
#[derive(Debug, Serialize)]
pub(crate) struct TenantResponse {
    pub(crate) id: Uuid,
    pub(crate) slug: String,
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) settings: Value,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

/// Tenant account delete request.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct DeleteTenantRequest {
    pub(crate) confirm_slug: Option<String>,
}

/// Tenant invitation response.
#[derive(Debug, Serialize)]
pub(crate) struct TenantInvitationResponse {
    pub(crate) id: Uuid,
    pub(crate) tenant_id: Uuid,
    pub(crate) user_id: Uuid,
    pub(crate) email: String,
    pub(crate) role: TenantUserRole,
    pub(crate) expires_at: DateTime<Utc>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) delivery_sent: bool,
}

pub(crate) struct CreatedInvitation {
    pub(crate) response: TenantInvitationResponse,
    pub(crate) tenant_name: String,
    pub(crate) token: SecretString,
}

pub(crate) enum ApplicationError {
    BadRequest(&'static str),
    Conflict(&'static str),
    NotFound(&'static str),
    Internal(String),
}
