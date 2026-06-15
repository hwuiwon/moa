//! Query helpers for mapping `PostgreSQL` rows into MOA core types.
use chrono::{DateTime, Utc};
use moa_core::{
    ApprovalRule, EventType, LearningEntry, MoaError, ModelId, Platform, PolicyAction, PolicyScope,
    Result, SegmentAssessment, SegmentId, SessionId, SessionMeta, SessionStatus, SessionSummary,
    TaskSegment, WorkspaceId,
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
