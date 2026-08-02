//! OpenFGA client and transactional outbox support for MOA authorization.

pub mod awakeable;
pub mod client;
pub mod error;
pub mod outbox;
pub mod poller;
pub mod require;

pub use awakeable::{AwakeableResolveError, AwakeableResolver};
pub use client::{FgaClient, FgaConfig, FgaTuple, SecurityAudit};
pub use error::AuthzError;
pub use outbox::{enqueue, enqueue_batch, enqueue_raw};
pub use poller::{OutboxPoller, PollerConfig, PollerHandle};
pub use require::{AuthzCheckError, fga_subject, require_authz, require_authz_with_delegation};
