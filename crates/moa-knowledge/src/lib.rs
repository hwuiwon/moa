//! Tenant knowledge-base domain, provider, parser, and ingestion seams.

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
pub mod repository;
pub mod semantic_graph;
pub mod semantic_graph_model;

pub use error::{Error, Result};
