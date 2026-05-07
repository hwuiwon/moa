//! Query helpers for mapping `PostgreSQL` rows into MOA core types.
use chrono::{DateTime, Utc};
use moa_core::{
    ApprovalRule, CatalogIntent, EventType, IntentSource, IntentStatus, LearningEntry, MoaError,
    ModelId, PendingSignal, PendingSignalId, PendingSignalType, Platform, PolicyAction,
    PolicyScope, ResolutionScore, Result, SegmentId, SessionId, SessionMeta, SessionStatus,
    SessionSummary, TaskSegment, TenantIntent, WorkspaceId,
};
use sqlx::{Row, postgres::PgRow};
use uuid::Uuid;

mod columns;
mod enums;
mod error;
mod rows;

pub(crate) use columns::*;
pub(crate) use enums::*;
pub(crate) use error::*;
pub(crate) use rows::*;
