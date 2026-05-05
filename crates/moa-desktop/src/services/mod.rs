//! Bridges tokio-based MOA backend services into the GPUI executor.
//!
//! [`ServiceBridge`] owns a dedicated tokio runtime plus the MOA [`ChatRuntime`]
//! facade. Views access it via [`ServiceBridgeHandle`] stored as a gpui global
//! and spawn async work through [`ServiceBridge::spawn_into`].

pub mod bridge;
pub mod init;
pub mod runtime;

pub use bridge::{ServiceBridge, ServiceBridgeHandle, ServiceStatus};
