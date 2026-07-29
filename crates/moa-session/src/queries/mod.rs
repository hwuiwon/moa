//! Query helpers for mapping `PostgreSQL` rows into MOA core types.
use chrono::{DateTime, Utc};
use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionPolicyRule,
    types::action_policy::ActionRuleScope, types::action_policy::CallOrigin,
    types::channel::SessionChannelBindingId, types::contact::ContactId, types::contact::ContactRef,
    types::contact::ContactVerificationState, types::contact::SessionActorRef,
    types::experience::ExperienceAttribution, types::experience::ExperienceRecord,
    types::experience::LearningCandidate, types::experience::TaskFingerprint,
    types::experience::TaskStrategySuccessRate, types::identifiers::ModelId,
    types::identifiers::SegmentId, types::identifiers::SessionId, types::identifiers::TenantId,
    types::identifiers::UserId, types::learning::LearningEntry,
    types::segment_assessment::SegmentAssessment, types::segments::TaskSegment,
    types::session::SessionMeta, types::session::SessionSummary,
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
