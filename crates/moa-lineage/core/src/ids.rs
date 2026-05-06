//! Identifier newtypes for lineage records.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One lineage turn identifier per agent turn.
///
/// Retrieval, context, and generation records for the same agent turn share
/// this ID so `moa explain` can render a turn tree from append-only rows.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(pub Uuid);

impl TurnId {
    /// Creates a UUIDv7 turn identifier.
    #[must_use]
    pub fn new_v7() -> Self {
        Self(Uuid::now_v7())
    }
}

/// Stable identifier for one lineage record payload.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LineageRecordId(pub Uuid);

impl LineageRecordId {
    /// Creates a UUIDv7 lineage record identifier.
    #[must_use]
    pub fn new_v7() -> Self {
        Self(Uuid::now_v7())
    }
}
