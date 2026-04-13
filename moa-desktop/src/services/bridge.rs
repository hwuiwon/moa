//! ServiceBridge entity bridging tokio-backed MOA services into GPUI views.

use std::sync::Arc;

use gpui::{App, Context, Entity, Global};
use moa_core::MoaConfig;
use moa_runtime::ChatRuntime;
use tokio::runtime::{Handle, Runtime};

use super::{init::InitializedServices, runtime::build_tokio_runtime};

/// Lifecycle state of the backend services.
#[derive(Clone, Debug)]
pub enum ServiceStatus {
    /// Config is loading and services are being constructed.
    Initializing,
    /// All required services are ready for use.
    Ready,
    /// A non-recoverable error occurred during initialization.
    Error(String),
    /// The runtime booted but some optional services failed.
    #[allow(dead_code)]
    Degraded { message: String },
}

/// Backend services accessible to views once initialization completes.
pub struct ServiceBridge {
    tokio: Arc<Runtime>,
    status: ServiceStatus,
    config: Option<MoaConfig>,
    chat_runtime: Option<ChatRuntime>,
}

impl ServiceBridge {
    /// Constructs an empty bridge that boots the tokio runtime immediately and
    /// reports [`ServiceStatus::Initializing`] until services are attached.
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            tokio: build_tokio_runtime()?,
            status: ServiceStatus::Initializing,
            config: None,
            chat_runtime: None,
        })
    }

    /// Returns a cloneable handle to the tokio runtime for spawning backend
    /// futures from GPUI tasks.
    pub fn tokio_handle(&self) -> Handle {
        self.tokio.handle().clone()
    }

    /// Current status snapshot.
    pub fn status(&self) -> &ServiceStatus {
        &self.status
    }

    /// Returns the loaded config once initialization has completed.
    #[allow(dead_code)]
    pub fn config(&self) -> Option<&MoaConfig> {
        self.config.as_ref()
    }

    /// Clones the [`ChatRuntime`] facade if services are ready.
    pub fn chat_runtime(&self) -> Option<ChatRuntime> {
        self.chat_runtime.clone()
    }

    /// Records a successful initialization.
    pub fn mark_ready(&mut self, services: InitializedServices, cx: &mut Context<Self>) {
        self.config = Some(services.config);
        self.chat_runtime = Some(services.chat_runtime);
        self.status = ServiceStatus::Ready;
        cx.notify();
    }

    /// Records an initialization failure.
    pub fn mark_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.status = ServiceStatus::Error(message);
        cx.notify();
    }
}

/// Helper that bridges a tokio future into an entity update on the GPUI thread.
///
/// The future runs on the shared tokio runtime; once complete, `update` is
/// invoked on `entity` via GPUI's main-thread scheduler.
pub fn spawn_into<T, Fut, R, F>(
    cx: &mut App,
    handle: Handle,
    entity: Entity<T>,
    fut: Fut,
    update: F,
) where
    T: 'static,
    R: Send + 'static,
    Fut: std::future::Future<Output = R> + Send + 'static,
    F: FnOnce(&mut T, R, &mut Context<T>) + 'static,
{
    cx.spawn(async move |cx| {
        let Ok(result) = handle.spawn(fut).await else {
            return;
        };
        let _ = entity.update(cx, |this, cx| {
            update(this, result, cx);
            cx.notify();
        });
    })
    .detach();
}

/// Global wrapper so any view can retrieve the [`ServiceBridge`] entity.
#[derive(Clone)]
pub struct ServiceBridgeHandle(pub Entity<ServiceBridge>);

impl ServiceBridgeHandle {
    /// Fetches the registered global bridge handle.
    pub fn global(cx: &App) -> Self {
        cx.global::<Self>().clone()
    }

    /// Returns the inner entity.
    pub fn entity(&self) -> &Entity<ServiceBridge> {
        &self.0
    }
}

impl Global for ServiceBridgeHandle {}
