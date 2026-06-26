//! Document parser trait and parser adapters.

use async_trait::async_trait;

use crate::{
    domain::{ParseInput, ParsedDocument},
    error::Result,
};

pub mod llamaparse;
pub mod native;
pub mod reducto;
pub mod unstructured;

/// Structure-aware parser seam for tenant knowledge ingestion.
#[async_trait]
pub trait DocumentParser: Send + Sync {
    /// Parses one source object into normalized document elements.
    async fn parse(&self, input: ParseInput) -> Result<ParsedDocument>;
}
