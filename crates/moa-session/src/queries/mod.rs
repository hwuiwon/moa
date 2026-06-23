//! Query helpers for mapping `PostgreSQL` rows into MOA core types.
use chrono::{DateTime, Utc};
use moa_core::{
    ActionPolicyRule, ActionRuleScope, ContactId, ContactRef, ContactVerificationState,
    ExperienceAttribution, ExperienceRecord, LearningCandidate, LearningEntry, MoaError, ModelId,
    Result, SegmentAssessment, SegmentId, SessionActorRef, SessionChannelBindingId, SessionId,
    SessionMeta, SessionSummary, TaskFingerprint, TaskSegment, TaskStrategySuccessRate, TenantId,
    UserId, WorkspaceId,
};
use sha2::{Digest, Sha256};
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
