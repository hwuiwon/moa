//! OpenFGA client and transactional outbox support for MOA authorization.

pub mod client;
pub mod error;
pub mod outbox;
pub mod poller;
pub mod schema;

pub use client::{FgaClient, FgaConfig, FgaTuple};
pub use error::AuthzError;
pub use outbox::enqueue;
pub use poller::{OutboxPoller, PollerConfig, PollerHandle};
