//! Typed compliance decision lineage records.
//!
//! The canonical serialization lives in `moa-lineage-core`; this module
//! re-exports those types so audit callers have a single crate to import.

pub use moa_lineage_core::{
    AclFilterDecision, DecisionKind, DecisionRecord, PiiRedactionDecision, PrivacyEraseDecision,
    PrivacyExportDecision, ScopeEnforcementDecision,
};
