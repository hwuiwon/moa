//! Graph-memory retrieval engine for MOA.
//!
//! This crate owns the memory-retrieval pipeline and the query planner that
//! drives it: hybrid vector/lexical/graph retrieval, ranking and fusion,
//! evidence-window admission, enrichment, and the strategy/entity planning that
//! seeds each retrieval. It is the self-contained bottom layer of the context
//! pipeline, split out of `moa-brain` so that consumers which only need the
//! retrieval and planning types do not rebuild the full brain crate.
//!
//! The [`retrieval`] and [`planning`] modules form a cooperating pair: planning
//! classifies a query and seeds it, and retrieval executes the planned query.

pub mod engine;
pub mod planning;
pub mod retrieval;
