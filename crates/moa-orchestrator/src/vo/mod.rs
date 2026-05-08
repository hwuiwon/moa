//! Shared plumbing for Restate virtual-object state.

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

    /// Persists state to an exclusive context.
    fn persist_into(&self, ctx: &ObjectContext<'_>);
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
