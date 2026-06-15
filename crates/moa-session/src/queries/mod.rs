//! Query helpers for mapping `PostgreSQL` rows into MOA core types.
use chrono::{DateTime, Utc};
use moa_core::{
    ApprovalRule, AttributionEffect, AttributionSubjectType, EventType, ExperienceAttribution,
    ExperienceRecord, LearningCandidate, LearningCandidateStatus, LearningCandidateType,
    LearningEntry, LearningRiskClass, MoaError, ModelId, Platform, PolicyAction, PolicyScope,
    Result, SegmentAssessment, SegmentId, SegmentOutcome, SessionId, SessionMeta, SessionStatus,
    SessionSummary, TaskFingerprint, TaskSegment, TaskStrategySuccessRate, UserId, WorkspaceId,
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
