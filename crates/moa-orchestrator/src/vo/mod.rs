//! Shared plumbing for Restate virtual-object state.

use std::time::Duration;

use restate_sdk::context::RequestTarget;
use restate_sdk::prelude::*;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Read-side abstraction over `ObjectContext` and `SharedObjectContext`.
///
/// Handlers frequently need to load durable state from either the exclusive
/// (`ObjectContext`) or the read-only (`SharedObjectContext`) variant of a
/// Restate object context. Both expose the same `get` method with identical
/// semantics for reads; this trait lets VOs write one `load_from` and reuse it
/// from both kinds of handler.
#[allow(async_fn_in_trait)]
pub(crate) trait VoReader {
    /// Loads one JSON-backed value from Restate object state.
    async fn get_json<T>(&self, key: &str) -> Result<Option<T>, HandlerError>
    where
        T: DeserializeOwned + 'static;
}

impl<'a> VoReader for ObjectContext<'a> {
    async fn get_json<T>(&self, key: &str) -> Result<Option<T>, HandlerError>
    where
        T: DeserializeOwned + 'static,
    {
        Ok(self.get::<Json<T>>(key).await?.map(Json::into_inner))
    }
}

impl<'a> VoReader for SharedObjectContext<'a> {
    async fn get_json<T>(&self, key: &str) -> Result<Option<T>, HandlerError>
    where
        T: DeserializeOwned + 'static,
    {
        Ok(self.get::<Json<T>>(key).await?.map(Json::into_inner))
    }
}

/// State that can be loaded from and persisted to a Restate virtual object.
#[allow(async_fn_in_trait)]
pub(crate) trait VoState: Default + Sized {
    /// Loads state from any reader, exclusive or shared.
    async fn load_from<R: VoReader>(reader: &R) -> Result<Self, HandlerError>;

    /// Persists all state keys to an exclusive context.
    fn persist_into(&self, ctx: &ObjectContext<'_>);

    /// Persists only the keys that differ from `baseline`.
    ///
    /// Writing an unchanged key is a durable no-op that still journals a state
    /// mutation, so skipping unchanged keys is replay-safe (the key retains its
    /// previously persisted value) and avoids re-journaling large fields such as
    /// buffered history on mutations that never touched them. The default writes
    /// everything for VOs that have not opted into dirty-tracking.
    fn persist_changes(&self, ctx: &ObjectContext<'_>, baseline: &Self) {
        let _ = baseline;
        self.persist_into(ctx);
    }
}

/// Load-mutate-persist guard that persists only the keys a handler changed.
///
/// Captures the loaded state as a baseline, dereferences to the live state for
/// in-handler mutation, and on [`Tracked::persist`] writes only the keys that
/// actually changed via [`VoState::persist_changes`].
pub(crate) struct Tracked<S> {
    baseline: S,
    current: S,
}

impl<S: VoState + Clone> Tracked<S> {
    /// Loads state and snapshots it as the dirty-tracking baseline.
    pub(crate) async fn load<R: VoReader>(reader: &R) -> Result<Self, HandlerError> {
        let current = S::load_from(reader).await?;
        Ok(Self {
            baseline: current.clone(),
            current,
        })
    }

    /// Persists only the keys changed since load.
    pub(crate) fn persist(&self, ctx: &ObjectContext<'_>) {
        self.current.persist_changes(ctx, &self.baseline);
    }
}

impl<S> std::ops::Deref for Tracked<S> {
    type Target = S;

    fn deref(&self) -> &Self::Target {
        &self.current
    }
}

impl<S> std::ops::DerefMut for Tracked<S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.current
    }
}

/// Sets `key` from `value` only when it differs from the baseline `previous`.
pub(crate) fn set_changed_opt<T>(
    ctx: &ObjectContext<'_>,
    key: &str,
    value: Option<&T>,
    previous: Option<&T>,
) where
    T: Clone + Serialize + PartialEq + 'static,
{
    if value != previous {
        set_or_clear_opt(ctx, key, value);
    }
}

/// Sets `key` from `values` only when they differ from the baseline `previous`.
pub(crate) fn set_changed_vec<T>(ctx: &ObjectContext<'_>, key: &str, values: &[T], previous: &[T])
where
    T: Clone + Serialize + PartialEq + 'static,
{
    if values != previous {
        set_or_clear_vec(ctx, key, values);
    }
}

/// Sets `key` from a scalar only when it differs from the baseline `previous`.
pub(crate) fn set_changed_scalar<T>(
    ctx: &ObjectContext<'_>,
    key: &str,
    value: T,
    previous: &T,
    empty_sentinel: T,
) where
    T: PartialEq + Serialize + Clone + 'static,
{
    if value != *previous {
        set_or_clear_scalar(ctx, key, value, empty_sentinel);
    }
}

/// Sets `key` when `value` is `Some`, clears it otherwise.
pub(crate) fn set_or_clear_opt<T>(ctx: &ObjectContext<'_>, key: &str, value: Option<&T>)
where
    T: Clone + Serialize + 'static,
{
    match value {
        Some(value) => ctx.set(key, Json::from(value.clone())),
        None => ctx.clear(key),
    }
}

/// Sets `key` when `values` is non-empty, clears it otherwise.
pub(crate) fn set_or_clear_vec<T>(ctx: &ObjectContext<'_>, key: &str, values: &[T])
where
    T: Clone + Serialize + 'static,
{
    if values.is_empty() {
        ctx.clear(key);
    } else {
        ctx.set(key, Json::from(values.to_vec()));
    }
}

/// Schedules a generation-guarded delayed self-call on the current virtual object.
///
/// This is the shared mechanism behind generation-fenced VO deadlines. It issues one Restate
/// delayed send back to `handler` on the *same* object key, carrying `generation` so
/// a call scheduled before a state transition can be recognized as stale and ignored
/// when it eventually fires. The idempotency key combines the object, handler, key,
/// generation, and a per-call `nonce`, so a replayed handler never double-schedules
/// the same logical deadline while successive deadlines stay distinct.
///
/// Modeled on the `CronJob` virtual object's delayed self-tick — same
/// `idempotency_key` + `send_after` Restate SDK calls — but kept generic over the
/// target handler, generation, payload, and delay so virtual objects share one
/// replay-safe scheduler rather than implementing divergent deadline machinery.
pub(crate) fn schedule_generation_guarded_self_call<T>(
    ctx: &ObjectContext<'_>,
    object_name: &str,
    handler: &str,
    generation: u64,
    nonce: impl std::fmt::Display,
    payload: Json<T>,
    delay: Duration,
) where
    T: Serialize + 'static,
{
    let key = ctx.key().to_string();
    let idempotency_key =
        format!("vo-self-call-{object_name}-{handler}-{key}-{generation}-{nonce}");
    ctx.request::<Json<T>, ()>(RequestTarget::object(object_name, key, handler), payload)
        .idempotency_key(idempotency_key)
        .send_after(delay);
}

/// Sets `key` to `value` unless it equals `empty_sentinel`, in which case clears.
pub(crate) fn set_or_clear_scalar<T>(
    ctx: &ObjectContext<'_>,
    key: &str,
    value: T,
    empty_sentinel: T,
) where
    T: PartialEq + Serialize + 'static,
{
    if value == empty_sentinel {
        ctx.clear(key);
    } else {
        ctx.set(key, Json::from(value));
    }
}
