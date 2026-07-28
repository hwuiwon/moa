//! Cooperative 429 cooldown and retry budget for one provider quota.
//!
//! Two anti-storm mechanisms live here:
//!
//! 1. **429 cooldown** — after a rate-limit response the provider records a
//!    pause deadline; subsequent calls short-circuit with a typed
//!    [`MoaError::RateLimited`] *without* sleeping, so a caller (or the failover
//!    wrapper) decides whether to wait or fail over rather than every task piling
//!    onto a provider that just said "slow down".
//! 2. **Retry budget** — in-call retries are allowed only while recent retry
//!    volume stays under a fraction of recent request volume, so a burst of
//!    rate-limited calls cannot amplify into a retry storm.
//!
//! With no shared quota attached both are process-local, and a fleet sharing one
//! API key divides the real budget across instances. Attaching a shared quota
//! ([`with_shared_quota`](RateGuard::with_shared_quota)) moves the cooldown
//! deadline and the retry-budget window into the runtime coordination store, so
//! one replica's 429 pauses the fleet and the retry budget is a fraction of
//! *fleet* request volume rather than a per-replica allowance that the replica
//! count multiplies.
//!
//! # Why the cooldown is not simply per model
//!
//! A 429 does not always mean "this model is busy". Providers return it for
//! account-level quota exhaustion too, and those two cases are not
//! distinguishable from the shared HTTP path here (see
//! [`RateLimitScope`]). Narrowing every cooldown to the model that happened to
//! be called would therefore let the other models on an exhausted credential
//! keep hammering it. So a cooldown is recorded at the scope the response
//! actually evidences, defaulting to the broader credential scope, while
//! admission checks the credential-scope and model-scope deadlines and honours
//! whichever is longer. An unnecessary short pause costs far less than
//! continuing to call a credential that is out of quota.
//!
//! The retry budget is deliberately credential-wide: it exists to bound retry
//! volume against one API key, and splitting it per model would hand each model
//! its own floor — the same multiplication this task removes across replicas.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use moa_config::{CoordinationFailurePolicy, ProviderPacingConfig};
use moa_core::error::{MoaError, Result};
use moa_core::traits::RuntimeCacheStore;
use tokio::time::Instant;

use super::concurrency_factory::{
    CoordinatedControl, QuotaIdentity, record_coordination_degraded, record_coordination_rejected,
};

/// Model label used before a call has resolved its model.
const UNSCOPED_MODEL: &str = "unscoped";

/// Key label standing for "the whole credential", used by credential-scoped
/// cooldowns and by the credential-wide retry budget.
///
/// `*` cannot collide with a provider model id, so a credential-scoped entry can
/// never be confused with a model named in a request.
const CREDENTIAL_SCOPE_LABEL: &str = "*";

/// Which quota a rate-limit response evidences.
///
/// Providers use 429 for both "this model is over its per-minute rate" and "this
/// account is out of quota", and the shared HTTP path that records the cooldown
/// sees only a status, headers, and an opaque body — it does not parse each
/// vendor's rate-limit taxonomy. Every caller in the tree therefore records
/// [`Credential`](Self::Credential) today: it is the conservative answer, and
/// guessing [`Model`](Self::Model) wrongly is precisely the failure that lets
/// the rest of an exhausted credential keep calling. An adapter that gains a
/// reliable signal can record the narrower scope without any other change,
/// because admission already reads both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RateLimitScope {
    /// The whole credential/account quota. The default when nothing distinguishes.
    Credential,
    /// One model on that credential.
    Model,
}

impl RateLimitScope {
    /// The scope to record for a rate-limit response nothing could classify.
    ///
    /// Every production caller goes through this one function rather than
    /// naming a variant, so the conservative default is a single decision with
    /// a single test, not a constant repeated at four call sites where one
    /// could drift.
    pub(crate) const fn unclassified() -> Self {
        Self::Credential
    }
}

/// Upper bound on per-scope local state kept by one guard.
///
/// Model ids come from the fixed provider catalog, so this is generous; it
/// exists so a caller that somehow passes unbounded model strings cannot grow
/// the map without limit.
const MAX_TRACKED_SCOPES: usize = 64;

/// Builds a typed rate-limit error for a provider that is in its 429 cooldown.
pub(crate) fn rate_limited_paused(remaining: Duration) -> MoaError {
    MoaError::RateLimited {
        retries: 0,
        message: format!(
            "provider paused after a recent rate limit; retry after {}ms",
            remaining.as_millis()
        ),
    }
}

/// Builds a typed rate-limit error for a saturated concurrency gate.
pub(crate) fn rate_limited_saturated(waited: Duration) -> MoaError {
    MoaError::RateLimited {
        retries: 0,
        message: format!(
            "provider concurrency gate saturated after waiting {}ms",
            waited.as_millis()
        ),
    }
}

/// Cloneable per-quota rate-limit guard; clones share one set of counters.
#[derive(Clone)]
pub(crate) struct RateGuard {
    inner: Arc<RateGuardInner>,
    pacing: ProviderPacingConfig,
    /// Model this handle is scoped to; it selects the model-scope cooldown this
    /// handle reads and can write. The credential-scope cooldown and the retry
    /// budget are shared by every model on the credential.
    model: Arc<str>,
    /// Rate class label for the shared keys (the provider call kind).
    class: &'static str,
    shared: Option<Arc<SharedGuard>>,
}

struct RateGuardInner {
    /// Process-local state keyed by model id, plus one entry under
    /// [`CREDENTIAL_SCOPE_LABEL`] for the credential-wide cooldown and retry
    /// budget. Used directly under local scope and as the bounded fallback when
    /// shared coordination degrades.
    states: Mutex<HashMap<Arc<str>, ScopeState>>,
}

/// Process-local cooldown and retry-budget state for one scope.
#[derive(Debug, Clone, Copy)]
struct ScopeState {
    /// Cooldown deadline; `None` means not paused.
    pause_until: Option<Instant>,
    /// When the current retry-budget window opened.
    window_start: Instant,
    requests: u64,
    retries: u64,
}

impl ScopeState {
    fn new(now: Instant) -> Self {
        Self {
            pause_until: None,
            window_start: now,
            requests: 0,
            retries: 0,
        }
    }

    /// Returns whether this entry still carries state worth keeping.
    fn is_active(&self, now: Instant, window: Duration) -> bool {
        self.pause_until.is_some_and(|deadline| deadline > now)
            || now.saturating_duration_since(self.window_start) < window
    }
}

/// Fleet-shared cooldown and retry-budget state for one credential's quota.
struct SharedGuard {
    store: Arc<dyn RuntimeCacheStore>,
    identity: QuotaIdentity,
    on_failure: CoordinationFailurePolicy,
}

impl RateGuard {
    /// Builds a process-local guard.
    pub(crate) fn new(pacing: ProviderPacingConfig) -> Self {
        Self {
            inner: Arc::new(RateGuardInner {
                states: Mutex::new(HashMap::new()),
            }),
            pacing,
            model: Arc::from(UNSCOPED_MODEL),
            class: "chat",
            shared: None,
        }
    }

    /// Moves cooldown and retry budget into the coordination store.
    ///
    /// A `None` store leaves both process-local, which is the correct behavior
    /// for a deliberate single-node deployment.
    pub(crate) fn with_shared_quota(
        mut self,
        store: Option<Arc<dyn RuntimeCacheStore>>,
        identity: QuotaIdentity,
        on_failure: CoordinationFailurePolicy,
    ) -> Self {
        self.shared = store.map(|store| {
            Arc::new(SharedGuard {
                store,
                identity,
                on_failure,
            })
        });
        self
    }

    /// Overrides the rate class label used in the shared keys.
    pub(crate) fn with_class(mut self, class: &'static str) -> Self {
        self.class = class;
        self
    }

    /// Returns a handle scoped to one resolved model.
    ///
    /// The model decides which model-scope cooldown this handle reads and can
    /// write; the credential-scope cooldown and the retry budget are shared by
    /// every model on the credential. The returned handle shares this guard's
    /// process-local state.
    pub(crate) fn for_model(&self, model: &str) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            pacing: self.pacing.clone(),
            model: Arc::from(model),
            class: self.class,
            shared: self.shared.clone(),
        }
    }

    /// Returns the remaining 429 cooldown for this quota, or `None` when clear.
    ///
    /// Honours the longer of the credential-scope and model-scope deadlines, so
    /// a credential-wide pause cannot be escaped by switching models and a
    /// model-scoped pause does not stall the rest of the credential.
    ///
    /// # Errors
    ///
    /// Returns a typed rate-limit error only when the cooldown cannot be read
    /// from the coordination store and the policy is `fail_closed`.
    pub(crate) async fn pause_remaining(&self) -> Result<Option<Duration>> {
        let Some(shared) = self.shared.as_ref() else {
            return Ok(self.local_pause_remaining());
        };
        let started = Instant::now();
        // Both scopes are read together so admission pays one round trip's
        // latency rather than two.
        let credential_key = self.cooldown_key(shared, RateLimitScope::Credential);
        let model_key = self.cooldown_key(shared, RateLimitScope::Model);
        let (credential, model) = tokio::join!(
            shared.store.cooldown_remaining(&credential_key),
            shared.store.cooldown_remaining(&model_key),
        );
        match credential.and_then(|credential| Ok(credential.max(model?))) {
            Ok(remaining) if remaining.is_zero() => Ok(None),
            Ok(remaining) => Ok(Some(remaining)),
            Err(error) => {
                if shared.on_failure.rejects_admission() {
                    record_coordination_rejected(
                        shared.identity.provider(),
                        CoordinatedControl::Cooldown,
                        &error,
                    );
                    return Err(MoaError::RateLimited {
                        retries: 0,
                        message: format!(
                            "provider cooldown coordination is unavailable and the \
                             coordination-failure policy is fail_closed: {error}"
                        ),
                    });
                }
                record_coordination_degraded(
                    shared.identity.provider(),
                    CoordinatedControl::Cooldown,
                    started.elapsed(),
                    &error,
                );
                Ok(self.local_pause_remaining())
            }
        }
    }

    /// Records a rate-limit response at the scope it evidences.
    ///
    /// Callers that cannot tell a model-scoped rate limit from account-level
    /// quota exhaustion must pass [`RateLimitScope::Credential`]; see that type
    /// for why that is the safe direction to be wrong in.
    ///
    /// A provider-supplied `Retry-After` is capped at the configured maximum, so
    /// one hostile header cannot pause a fleet's access to a credential for an
    /// unbounded time. The local deadline is always recorded too, so a later
    /// coordination failure degrades onto state that is already correct.
    pub(crate) async fn record_rate_limited(
        &self,
        retry_after: Option<Duration>,
        scope: RateLimitScope,
    ) {
        let cooldown = retry_after
            .filter(|delay| !delay.is_zero())
            .unwrap_or_else(|| Duration::from_millis(self.pacing.default_cooldown_ms))
            .min(Duration::from_millis(self.pacing.max_cooldown_ms));
        let now = Instant::now();
        self.with_state(self.scope_label(scope), now, |state| {
            let deadline = now + cooldown;
            // Never shorten an active cooldown set by a concurrent call.
            if state.pause_until.is_none_or(|current| current < deadline) {
                state.pause_until = Some(deadline);
            }
        });
        let Some(shared) = self.shared.as_ref() else {
            return;
        };
        // Best effort: this runs on a path that already failed, so a coordination
        // failure here degrades onto the local deadline recorded above rather
        // than turning into a second error.
        if let Err(error) = shared
            .store
            .extend_cooldown(&self.cooldown_key(shared, scope), cooldown)
            .await
        {
            record_coordination_degraded(
                shared.identity.provider(),
                CoordinatedControl::Cooldown,
                Duration::ZERO,
                &error,
            );
        }
    }

    /// Counts one outbound request toward the retry-budget window.
    ///
    /// # Errors
    ///
    /// Returns a typed rate-limit error only when the shared window cannot be
    /// updated and the policy is `fail_closed`.
    pub(crate) async fn note_request(&self) -> Result<()> {
        let now = Instant::now();
        let Some(shared) = self.shared.as_ref() else {
            self.note_local_request(now);
            return Ok(());
        };
        let started = now;
        match shared
            .store
            .note_windowed_request(&self.retry_key(shared), self.retry_window())
            .await
        {
            Ok(_) => Ok(()),
            Err(error) => {
                if shared.on_failure.rejects_admission() {
                    record_coordination_rejected(
                        shared.identity.provider(),
                        CoordinatedControl::RetryBudget,
                        &error,
                    );
                    return Err(MoaError::RateLimited {
                        retries: 0,
                        message: format!(
                            "provider retry-budget coordination is unavailable and the \
                             coordination-failure policy is fail_closed: {error}"
                        ),
                    });
                }
                record_coordination_degraded(
                    shared.identity.provider(),
                    CoordinatedControl::RetryBudget,
                    started.elapsed(),
                    &error,
                );
                self.note_local_request(now);
                Ok(())
            }
        }
    }

    /// Returns whether another in-call retry is within budget, consuming one unit
    /// of budget when it returns `true`.
    ///
    /// A coordination failure under `fail_closed` denies the retry: refusing to
    /// retry is the bounded answer, since an uncoordinated retry allowance is
    /// exactly what multiplies into a fleet-wide storm.
    pub(crate) async fn allow_retry(&self) -> bool {
        let Some(shared) = self.shared.as_ref() else {
            return self.allow_local_retry(Instant::now());
        };
        let started = Instant::now();
        match shared
            .store
            .try_consume_retry_budget(
                &self.retry_key(shared),
                self.retry_window(),
                self.pacing.retry_budget_percent,
                self.pacing.retry_budget_floor,
            )
            .await
        {
            Ok(decision) => decision.allowed,
            Err(error) => {
                if shared.on_failure.rejects_admission() {
                    record_coordination_rejected(
                        shared.identity.provider(),
                        CoordinatedControl::RetryBudget,
                        &error,
                    );
                    return false;
                }
                record_coordination_degraded(
                    shared.identity.provider(),
                    CoordinatedControl::RetryBudget,
                    started.elapsed(),
                    &error,
                );
                self.allow_local_retry(started)
            }
        }
    }

    fn retry_window(&self) -> Duration {
        Duration::from_millis(self.pacing.retry_budget_window_ms)
    }

    /// Returns the state label one cooldown scope is stored under.
    fn scope_label(&self, scope: RateLimitScope) -> Arc<str> {
        match scope {
            RateLimitScope::Credential => Arc::from(CREDENTIAL_SCOPE_LABEL),
            RateLimitScope::Model => Arc::clone(&self.model),
        }
    }

    fn cooldown_key(&self, shared: &SharedGuard, scope: RateLimitScope) -> String {
        shared
            .identity
            .key("cooldown", &self.scope_label(scope), self.class)
    }

    /// The retry budget is one allowance for the whole credential, so its key
    /// deliberately carries no model.
    fn retry_key(&self, shared: &SharedGuard) -> String {
        shared
            .identity
            .key("retry-budget", CREDENTIAL_SCOPE_LABEL, self.class)
    }

    fn local_pause_remaining(&self) -> Option<Duration> {
        let now = Instant::now();
        let remaining = |scope| {
            self.with_state(self.scope_label(scope), now, |state| {
                state
                    .pause_until
                    .filter(|deadline| *deadline > now)
                    .map(|deadline| deadline.saturating_duration_since(now))
                    .unwrap_or(Duration::ZERO)
            })
        };
        let longest = remaining(RateLimitScope::Credential).max(remaining(RateLimitScope::Model));
        (!longest.is_zero()).then_some(longest)
    }

    fn note_local_request(&self, now: Instant) {
        let window = self.retry_window();
        self.with_state(Arc::from(CREDENTIAL_SCOPE_LABEL), now, |state| {
            rotate(state, now, window);
            state.requests = state.requests.saturating_add(1);
        });
    }

    fn allow_local_retry(&self, now: Instant) -> bool {
        let window = self.retry_window();
        let percent = u64::from(self.pacing.retry_budget_percent);
        let floor = self.pacing.retry_budget_floor;
        self.with_state(Arc::from(CREDENTIAL_SCOPE_LABEL), now, |state| {
            rotate(state, now, window);
            let budget = state
                .requests
                .saturating_mul(percent)
                .saturating_div(100)
                .max(floor);
            if state.retries < budget {
                state.retries = state.retries.saturating_add(1);
                true
            } else {
                false
            }
        })
    }

    /// Runs `apply` against the state stored under `label`, creating it on first
    /// use. `label` is either a model id or [`CREDENTIAL_SCOPE_LABEL`].
    fn with_state<T>(
        &self,
        label: Arc<str>,
        now: Instant,
        apply: impl FnOnce(&mut ScopeState) -> T,
    ) -> T {
        let window = self.retry_window();
        let mut states = self
            .inner
            .states
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if states.len() >= MAX_TRACKED_SCOPES && !states.contains_key(&label) {
            states.retain(|_, state| state.is_active(now, window));
            if states.len() >= MAX_TRACKED_SCOPES {
                states.clear();
            }
        }
        let state = states.entry(label).or_insert_with(|| ScopeState::new(now));
        apply(state)
    }
}

/// Resets the retry-budget counters when the sliding window has elapsed.
fn rotate(state: &mut ScopeState, now: Instant, window: Duration) {
    if now.saturating_duration_since(state.window_start) >= window {
        state.window_start = now;
        state.requests = 0;
        state.retries = 0;
    }
}

#[cfg(test)]
mod tests {
    use moa_runtime_store::MemoryRuntimeCacheStore;

    use crate::core::coordination_test_support::FailingStore;

    use super::*;

    const MODEL: &str = "claude-sonnet-4-6";

    fn local_guard() -> RateGuard {
        RateGuard::new(ProviderPacingConfig::default()).for_model(MODEL)
    }

    fn shared_guard(
        store: Arc<dyn RuntimeCacheStore>,
        on_failure: CoordinationFailurePolicy,
    ) -> RateGuard {
        RateGuard::new(ProviderPacingConfig::default())
            .with_shared_quota(
                Some(store),
                QuotaIdentity::new("anthropic", "shared-credential"),
                on_failure,
            )
            .for_model(MODEL)
    }

    #[tokio::test(start_paused = true)]
    async fn pause_short_circuits_until_cooldown_elapses() {
        // Pins: a recorded 429 pauses the provider for the cooldown window and the
        // pause clears once the window elapses, without any sleeping.
        let guard = local_guard();
        assert!(guard.pause_remaining().await.expect("read pause").is_none());

        guard
            .record_rate_limited(Some(Duration::from_secs(10)), RateLimitScope::Credential)
            .await;
        assert!(
            guard.pause_remaining().await.expect("read pause").is_some(),
            "provider should be paused immediately after a 429"
        );

        tokio::time::advance(Duration::from_secs(11)).await;
        assert!(
            guard.pause_remaining().await.expect("read pause").is_none(),
            "pause should clear once the cooldown elapses"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn missing_retry_after_uses_the_configured_default_cooldown() {
        // Pins: a 429 with no Retry-After still pauses, for the configured default.
        let guard = local_guard();
        guard
            .record_rate_limited(None, RateLimitScope::Credential)
            .await;
        assert!(guard.pause_remaining().await.expect("read pause").is_some());

        let default = Duration::from_millis(ProviderPacingConfig::default().default_cooldown_ms);
        tokio::time::advance(default + Duration::from_millis(1)).await;
        assert!(guard.pause_remaining().await.expect("read pause").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_hostile_retry_after_is_capped_at_the_configured_maximum() {
        // Pins: one provider response cannot pause a credential for an unbounded
        // time — the cooldown is capped, so the fleet recovers on schedule.
        let guard = local_guard();
        guard
            .record_rate_limited(
                Some(Duration::from_secs(86_400)),
                RateLimitScope::Credential,
            )
            .await;

        let cap = Duration::from_millis(ProviderPacingConfig::default().max_cooldown_ms);
        tokio::time::advance(cap + Duration::from_millis(1)).await;
        assert!(
            guard.pause_remaining().await.expect("read pause").is_none(),
            "a day-long Retry-After must be capped at max_cooldown_ms"
        );
    }

    /// Two guards over one coordination store, standing in for two replicas.
    fn shared_replicas(store: Arc<dyn RuntimeCacheStore>) -> (RateGuard, RateGuard) {
        let build = |store| {
            RateGuard::new(ProviderPacingConfig::default()).with_shared_quota(
                Some(store),
                QuotaIdentity::new("anthropic", "shared-credential"),
                CoordinationFailurePolicy::BoundedDegraded,
            )
        };
        (build(Arc::clone(&store)), build(store))
    }

    #[tokio::test(start_paused = true)]
    async fn a_credential_scoped_429_cannot_be_escaped_by_switching_models() {
        // Pins the load-bearing property of the cooldown scope. A 429 that
        // evidences account-level quota exhaustion is recorded credential-wide,
        // so no model on that credential is callable — on this replica or any
        // other. Narrowing such a cooldown to the model that happened to be
        // called would let every other model keep hammering an exhausted key,
        // which is worse than the per-provider-instance behavior this replaced.
        let store: Arc<dyn RuntimeCacheStore> = Arc::new(MemoryRuntimeCacheStore::new());
        let (replica_a, replica_b) = shared_replicas(store);

        replica_a
            .for_model("claude-opus-4-6")
            .record_rate_limited(Some(Duration::from_secs(30)), RateLimitScope::Credential)
            .await;

        for model in ["claude-opus-4-6", "claude-haiku-4-5", "claude-sonnet-4-6"] {
            assert!(
                replica_b
                    .for_model(model)
                    .pause_remaining()
                    .await
                    .expect("read")
                    .is_some(),
                "{model} must observe the credential-wide pause on the other replica"
            );
        }

        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(
            replica_b
                .for_model("claude-haiku-4-5")
                .pause_remaining()
                .await
                .expect("read")
                .is_none(),
            "the credential-wide pause must clear on schedule rather than latch"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_model_scoped_cooldown_does_not_pause_the_rest_of_the_credential() {
        // Pins the other half: when a response does evidence a model-scoped
        // limit, the pause stays on that model — shared across replicas, but not
        // stalling the credential's other models. Admission reads both scopes,
        // so this and the credential-wide case coexist.
        let store: Arc<dyn RuntimeCacheStore> = Arc::new(MemoryRuntimeCacheStore::new());
        let (replica_a, replica_b) = shared_replicas(store);

        replica_a
            .for_model("claude-opus-4-6")
            .record_rate_limited(Some(Duration::from_secs(30)), RateLimitScope::Model)
            .await;

        assert!(
            replica_b
                .for_model("claude-opus-4-6")
                .pause_remaining()
                .await
                .expect("read")
                .is_some(),
            "the rate-limited model must be paused fleet-wide"
        );
        assert!(
            replica_b
                .for_model("claude-haiku-4-5")
                .pause_remaining()
                .await
                .expect("read")
                .is_none(),
            "a model-scoped pause must not stall the credential's other models"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn local_cooldowns_honour_both_scopes() {
        // Pins the same two-scope rule on the process-local path, which is both
        // the local-scope deployment and the degraded fallback: a credential
        // 429 pauses every model, a model 429 pauses only its own.
        let guard = RateGuard::new(ProviderPacingConfig::default());
        let opus = guard.for_model("claude-opus-4-6");
        let haiku = guard.for_model("claude-haiku-4-5");

        opus.record_rate_limited(Some(Duration::from_secs(30)), RateLimitScope::Model)
            .await;
        assert!(opus.pause_remaining().await.expect("read").is_some());
        assert!(
            haiku.pause_remaining().await.expect("read").is_none(),
            "a model-scoped cooldown must not leak to another model"
        );

        haiku
            .record_rate_limited(Some(Duration::from_secs(30)), RateLimitScope::Credential)
            .await;
        assert!(
            opus.pause_remaining().await.expect("read").is_some(),
            "a credential-scoped cooldown must pause every model"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_budget_allows_the_floor_then_fails_fast() {
        // Pins: retries are allowed up to the floor under low volume, then the
        // budget blocks further retries until request volume grows.
        let guard = local_guard();
        guard.note_request().await.expect("note request");
        for _ in 0..ProviderPacingConfig::default().retry_budget_floor {
            assert!(
                guard.allow_retry().await,
                "retries within the floor are allowed"
            );
        }
        assert!(
            !guard.allow_retry().await,
            "the retry budget must fail fast once the floor is exhausted at low volume"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_budget_scales_to_the_configured_percent_of_request_volume() {
        // Pins: under high request volume the budget grows to the configured
        // percentage of observed requests.
        let guard = local_guard();
        for _ in 0..1_000 {
            guard.note_request().await.expect("note request");
        }
        let mut allowed = 0;
        for _ in 0..1_000 {
            if guard.allow_retry().await {
                allowed += 1;
            }
        }
        assert_eq!(allowed, 200, "retry budget should be 20% of 1000 requests");
    }

    #[tokio::test(start_paused = true)]
    async fn one_replicas_429_pauses_every_replica_sharing_the_quota() {
        // Pins: the distinguishing behavior for cooldown. Two guards backed by one
        // coordination store share a pause: replica A's 429 short-circuits replica
        // B, which has its own untouched local state and would proceed under
        // process-local cooldown.
        let store: Arc<dyn RuntimeCacheStore> = Arc::new(MemoryRuntimeCacheStore::new());
        let replica_a = shared_guard(
            Arc::clone(&store),
            CoordinationFailurePolicy::BoundedDegraded,
        );
        let replica_b = shared_guard(
            Arc::clone(&store),
            CoordinationFailurePolicy::BoundedDegraded,
        );

        assert!(replica_b.pause_remaining().await.expect("read").is_none());
        replica_a
            .record_rate_limited(Some(Duration::from_secs(30)), RateLimitScope::Credential)
            .await;

        let remaining = replica_b
            .pause_remaining()
            .await
            .expect("read")
            .expect("replica B must observe replica A's cooldown");
        assert!(remaining <= Duration::from_secs(30) && remaining > Duration::from_secs(25));

        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(
            replica_b.pause_remaining().await.expect("read").is_none(),
            "the shared pause must clear for every replica once it elapses"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn uncoordinated_replicas_do_not_share_a_cooldown() {
        // Pins: the negative control. Without a shared quota, replica A's 429 is
        // invisible to replica B — the exact fleet behavior this task removes.
        let replica_a = RateGuard::new(ProviderPacingConfig::default()).for_model(MODEL);
        let replica_b = RateGuard::new(ProviderPacingConfig::default()).for_model(MODEL);

        replica_a
            .record_rate_limited(Some(Duration::from_secs(30)), RateLimitScope::Credential)
            .await;
        assert!(replica_a.pause_remaining().await.expect("read").is_some());
        assert!(
            replica_b.pause_remaining().await.expect("read").is_none(),
            "an uncoordinated replica cannot see another replica's cooldown"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_retry_budget_is_one_allowance_for_the_whole_credential() {
        // Pins: the retry budget bounds retry volume against one API key, so
        // every model on that key draws from a single allowance. Keying it per
        // model would hand each model its own floor — the same multiplication
        // across models that this task removes across replicas.
        let store: Arc<dyn RuntimeCacheStore> = Arc::new(MemoryRuntimeCacheStore::new());
        let (replica_a, replica_b) = shared_replicas(store);
        let floor = ProviderPacingConfig::default().retry_budget_floor;

        let opus = replica_a.for_model("claude-opus-4-6");
        opus.note_request().await.expect("note request");
        for _ in 0..floor {
            assert!(opus.allow_retry().await, "opus spends the shared floor");
        }

        assert!(
            !replica_b.for_model("claude-haiku-4-5").allow_retry().await,
            "a different model on the same credential must find the budget spent"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_retry_budget_is_one_fleet_wide_allowance() {
        // Pins: the distinguishing behavior for the retry budget. Two replicas
        // sharing a quota draw from ONE floor-sized allowance; under process-local
        // budgets each would get its own floor, which is the multiplication this
        // task removes.
        let store: Arc<dyn RuntimeCacheStore> = Arc::new(MemoryRuntimeCacheStore::new());
        let replica_a = shared_guard(
            Arc::clone(&store),
            CoordinationFailurePolicy::BoundedDegraded,
        );
        let replica_b = shared_guard(
            Arc::clone(&store),
            CoordinationFailurePolicy::BoundedDegraded,
        );
        let floor = ProviderPacingConfig::default().retry_budget_floor;

        replica_a.note_request().await.expect("note request");
        let mut allowed_a = 0;
        for _ in 0..floor {
            if replica_a.allow_retry().await {
                allowed_a += 1;
            }
        }
        assert_eq!(allowed_a, floor, "replica A spends the shared floor");
        assert!(
            !replica_b.allow_retry().await,
            "replica B must find the fleet-wide retry budget already spent"
        );

        let uncoordinated = RateGuard::new(ProviderPacingConfig::default()).for_model(MODEL);
        assert!(
            uncoordinated.allow_retry().await,
            "an uncoordinated guard still has its own full budget, proving the \
             assertion above is about coordination and not about exhaustion"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shared_guard_degrades_to_local_state_when_the_store_fails() {
        // Pins: bounded_degraded keeps the guard usable on a coordination failure,
        // falling back to this replica's own cooldown and budget.
        let guard = shared_guard(
            Arc::new(FailingStore),
            CoordinationFailurePolicy::BoundedDegraded,
        );

        guard
            .note_request()
            .await
            .expect("degraded note must not fail");
        guard
            .record_rate_limited(Some(Duration::from_secs(10)), RateLimitScope::Credential)
            .await;
        assert!(
            guard
                .pause_remaining()
                .await
                .expect("degraded read")
                .is_some(),
            "the degraded guard must still observe its own cooldown"
        );
        assert!(
            guard.allow_retry().await,
            "the degraded guard must still allow bounded local retries"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shared_guard_fails_closed_when_the_store_fails() {
        // Pins: fail_closed rejects admission and denies retries rather than
        // running on an uncoordinated allowance.
        let guard = shared_guard(
            Arc::new(FailingStore),
            CoordinationFailurePolicy::FailClosed,
        );

        let error = guard
            .pause_remaining()
            .await
            .expect_err("fail_closed must reject a cooldown it cannot read");
        assert!(matches!(error, MoaError::RateLimited { .. }), "{error}");
        assert!(
            guard.note_request().await.is_err(),
            "fail_closed must reject a request it cannot count toward the fleet budget"
        );
        assert!(
            !guard.allow_retry().await,
            "fail_closed must deny retries it cannot coordinate"
        );
    }
}
