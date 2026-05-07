//! Shared provider plumbing used by vendor adapters.

pub mod factory;
pub(crate) mod http;
pub(crate) mod instrumentation;
pub mod models;
pub(crate) mod provider_tools;
pub(crate) mod retry;
pub mod router;
pub(crate) mod schema;
pub(crate) mod streaming;
