//! Tenant knowledge-base domain, provider, parser, and ingestion seams.

pub mod acl_key;
pub mod chunking;
pub mod contact_groups;
pub mod domain;
pub mod error;
pub mod graph_delta;
pub mod ingestion;
pub mod normalize;
pub mod observability;
pub mod parser;
pub mod providers;
pub mod rechunk;
pub mod repository;

pub use error::{Error, Result};
