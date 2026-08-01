//! Tenant-configurable agent resolution and runtime policy locking.

mod definition;
mod policy;
mod resolver;

pub use definition::{AgentInstallationBinding, AgentInstallationPointer};
pub use policy::AgentRuntimePolicy;
pub use resolver::AgentResolver;
