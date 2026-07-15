//! Connector-configured request pacing with process-wide fair admission.
//!
//! Connectors own their API semantics (retryable methods/statuses, auth, and
//! response decoding). This module owns the reusable mechanics: a token bucket
//! per connector quota scope and a global in-flight ceiling shared fairly
//! across scopes. The global ceiling is backpressure, not an API rate limit.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

const DEFAULT_GLOBAL_MAX_IN_FLIGHT: usize = 32;
const FAIRNESS_RECHECK_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Debug, PartialEq)]
pub struct ConnectorNetworkConfig {
    pub quota_scope: String,
    pub requests_per_second: f64,
    pub burst: f64,
    pub max_in_flight: usize,
    pub request_timeout: Duration,
    pub retry: RetryConfig,
}

impl ConnectorNetworkConfig {
    pub fn new(quota_scope: impl Into<String>, requests_per_second: f64, burst: f64) -> Self {
        Self {
            quota_scope: quota_scope.into(),
            requests_per_second,
            burst,
            max_in_flight: DEFAULT_GLOBAL_MAX_IN_FLIGHT,
            request_timeout: Duration::from_secs(30),
            retry: RetryConfig::default(),
        }
    }

    pub fn max_in_flight(mut self, max_in_flight: usize) -> Self {
        self.max_in_flight = max_in_flight.max(1);
        self
    }

    pub fn request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    pub fn retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    fn normalized(mut self) -> Self {
        if !self.requests_per_second.is_finite() || self.requests_per_second <= 0.0 {
            self.requests_per_second = 1.0;
        }
        if !self.burst.is_finite() || self.burst < 1.0 {
            self.burst = 1.0;
        }
        self.max_in_flight = self.max_in_flight.max(1);
        if self.request_timeout.is_zero() {
            self.request_timeout = Duration::from_secs(30);
        }
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryConfig {
    pub max_retries: usize,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl RetryConfig {
    pub fn exponential(
        max_retries: usize,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Self {
        Self {
            max_retries,
            initial_backoff,
            max_backoff: max_backoff.max(initial_backoff),
        }
    }

    pub fn backoff(&self, attempt: usize) -> Duration {
        let multiplier = 1_u32
            .checked_shl(attempt.min(31) as u32)
            .unwrap_or(u32::MAX);
        self.initial_backoff
            .saturating_mul(multiplier)
            .min(self.max_backoff)
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self::exponential(4, Duration::from_secs(1), Duration::from_secs(16))
    }
}

#[derive(Clone, Debug)]
pub struct ConnectorNetworkGate {
    orchestrator: NetworkOrchestrator,
    config: ConnectorNetworkConfig,
}

impl ConnectorNetworkGate {
    pub fn global(config: ConnectorNetworkConfig) -> Self {
        Self::new(global_network_orchestrator(), config)
    }

    pub fn new(orchestrator: NetworkOrchestrator, config: ConnectorNetworkConfig) -> Self {
        let config = config.normalized();
        orchestrator.register(&config);
        Self {
            orchestrator,
            config,
        }
    }

    pub fn acquire(&self) -> NetworkPermit {
        self.orchestrator.acquire(&self.config)
    }

    pub fn record_cooldown(&self, delay: Duration) {
        self.orchestrator
            .record_cooldown(&self.config.quota_scope, delay);
    }

    pub fn status(&self) -> ConnectorNetworkStatus {
        self.orchestrator.status(&self.config.quota_scope)
    }

    pub fn config(&self) -> &ConnectorNetworkConfig {
        &self.config
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConnectorNetworkStatus {
    pub waiting: usize,
    pub in_flight: usize,
    pub tokens: f64,
    pub burst: f64,
    pub requests_per_second: f64,
    pub cooldown_remaining: Option<Duration>,
    pub global_in_flight: usize,
    pub global_max_in_flight: usize,
}

#[derive(Clone, Debug)]
pub struct NetworkOrchestrator {
    inner: Arc<NetworkOrchestratorInner>,
}

#[derive(Debug)]
struct NetworkOrchestratorInner {
    max_in_flight: usize,
    state: Mutex<NetworkOrchestratorState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct NetworkOrchestratorState {
    in_flight: usize,
    scopes: BTreeMap<String, ScopeState>,
    rotation: VecDeque<String>,
}

#[derive(Clone, Debug)]
struct ScopeState {
    requests_per_second: f64,
    burst: f64,
    max_in_flight: usize,
    tokens: f64,
    last_refill: Instant,
    cooldown_until: Option<Instant>,
    in_flight: usize,
    waiting: usize,
}

impl ScopeState {
    fn new(config: &ConnectorNetworkConfig) -> Self {
        Self {
            requests_per_second: config.requests_per_second,
            burst: config.burst,
            max_in_flight: config.max_in_flight,
            tokens: config.burst,
            last_refill: Instant::now(),
            cooldown_until: None,
            in_flight: 0,
            waiting: 0,
        }
    }

    fn refill(&mut self, now: Instant) {
        if self.cooldown_until.is_some_and(|until| until > now) {
            return;
        }
        self.cooldown_until = None;
        let elapsed = now.saturating_duration_since(self.last_refill);
        self.tokens =
            (self.tokens + elapsed.as_secs_f64() * self.requests_per_second).min(self.burst);
        self.last_refill = now;
    }

    fn is_ready(&self, now: Instant) -> bool {
        self.waiting > 0
            && self.in_flight < self.max_in_flight
            && self.tokens >= 1.0
            && self.cooldown_until.is_none_or(|until| until <= now)
    }

    fn next_ready_delay(&self, now: Instant) -> Duration {
        if let Some(until) = self.cooldown_until.filter(|until| *until > now) {
            return until.saturating_duration_since(now);
        }
        if self.tokens < 1.0 {
            return Duration::from_secs_f64(
                (1.0 - self.tokens).max(0.0) / self.requests_per_second,
            );
        }
        FAIRNESS_RECHECK_INTERVAL
    }
}

impl NetworkOrchestrator {
    pub fn new(max_in_flight: usize) -> Self {
        Self {
            inner: Arc::new(NetworkOrchestratorInner {
                max_in_flight: max_in_flight.max(1),
                state: Mutex::new(NetworkOrchestratorState::default()),
                changed: Condvar::new(),
            }),
        }
    }

    fn register(&self, config: &ConnectorNetworkConfig) {
        let mut state = self.inner.state.lock().expect("network orchestrator lock");
        state
            .scopes
            .entry(config.quota_scope.clone())
            .or_insert_with(|| ScopeState::new(config));
    }

    fn acquire(&self, config: &ConnectorNetworkConfig) -> NetworkPermit {
        let started = Instant::now();
        let scope = config.quota_scope.clone();
        let mut state = self.inner.state.lock().expect("network orchestrator lock");
        if !state.scopes.contains_key(&scope) {
            state.scopes.insert(scope.clone(), ScopeState::new(config));
        }
        let scope_state = state.scopes.get_mut(&scope).expect("registered scope");
        scope_state.waiting = scope_state.waiting.saturating_add(1);
        if scope_state.waiting == 1 {
            state.rotation.push_back(scope.clone());
        }

        loop {
            let now = Instant::now();
            for scope_state in state.scopes.values_mut() {
                scope_state.refill(now);
            }
            let chosen = if state.in_flight < self.inner.max_in_flight {
                state.rotation.iter().find_map(|candidate| {
                    state
                        .scopes
                        .get(candidate)
                        .filter(|candidate_state| candidate_state.is_ready(now))
                        .map(|_| candidate.clone())
                })
            } else {
                None
            };

            if chosen.as_deref() == Some(scope.as_str()) {
                let position = state
                    .rotation
                    .iter()
                    .position(|candidate| candidate == &scope)
                    .expect("waiting scope is in rotation");
                state.rotation.remove(position);
                let scope_state = state.scopes.get_mut(&scope).expect("registered scope");
                scope_state.waiting = scope_state.waiting.saturating_sub(1);
                scope_state.in_flight = scope_state.in_flight.saturating_add(1);
                scope_state.tokens = (scope_state.tokens - 1.0).max(0.0);
                if scope_state.waiting > 0 {
                    state.rotation.push_back(scope.clone());
                }
                state.in_flight = state.in_flight.saturating_add(1);
                self.inner.changed.notify_all();
                return NetworkPermit {
                    inner: Arc::clone(&self.inner),
                    scope,
                    waited: started.elapsed(),
                };
            }

            let delay = state
                .scopes
                .get(&scope)
                .expect("registered scope")
                .next_ready_delay(now)
                .min(FAIRNESS_RECHECK_INTERVAL)
                .max(Duration::from_millis(1));
            let (next_state, _) = self
                .inner
                .changed
                .wait_timeout(state, delay)
                .expect("network orchestrator wait");
            state = next_state;
        }
    }

    fn record_cooldown(&self, scope: &str, delay: Duration) {
        let mut state = self.inner.state.lock().expect("network orchestrator lock");
        let Some(scope_state) = state.scopes.get_mut(scope) else {
            return;
        };
        let until = Instant::now() + delay;
        scope_state.cooldown_until = Some(
            scope_state
                .cooldown_until
                .map_or(until, |current| current.max(until)),
        );
        scope_state.tokens = 0.0;
        scope_state.last_refill = Instant::now();
        self.inner.changed.notify_all();
    }

    fn status(&self, scope: &str) -> ConnectorNetworkStatus {
        let mut state = self.inner.state.lock().expect("network orchestrator lock");
        let now = Instant::now();
        let global_in_flight = state.in_flight;
        let scope_state = state.scopes.get_mut(scope).expect("registered scope");
        scope_state.refill(now);
        ConnectorNetworkStatus {
            waiting: scope_state.waiting,
            in_flight: scope_state.in_flight,
            tokens: scope_state.tokens,
            burst: scope_state.burst,
            requests_per_second: scope_state.requests_per_second,
            cooldown_remaining: scope_state
                .cooldown_until
                .filter(|until| *until > now)
                .map(|until| until.saturating_duration_since(now)),
            global_in_flight,
            global_max_in_flight: self.inner.max_in_flight,
        }
    }
}

#[derive(Debug)]
pub struct NetworkPermit {
    inner: Arc<NetworkOrchestratorInner>,
    scope: String,
    waited: Duration,
}

impl NetworkPermit {
    pub fn waited(&self) -> Duration {
        self.waited
    }
}

impl Drop for NetworkPermit {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().expect("network orchestrator lock");
        state.in_flight = state.in_flight.saturating_sub(1);
        if let Some(scope_state) = state.scopes.get_mut(&self.scope) {
            scope_state.in_flight = scope_state.in_flight.saturating_sub(1);
        }
        self.inner.changed.notify_all();
    }
}

static GLOBAL_NETWORK_ORCHESTRATOR: OnceLock<NetworkOrchestrator> = OnceLock::new();

pub fn global_network_orchestrator() -> NetworkOrchestrator {
    GLOBAL_NETWORK_ORCHESTRATOR
        .get_or_init(|| NetworkOrchestrator::new(DEFAULT_GLOBAL_MAX_IN_FLIGHT))
        .clone()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::{ConnectorNetworkConfig, ConnectorNetworkGate, NetworkOrchestrator, RetryConfig};
    use std::time::{Duration, Instant};

    #[test]
    fn retry_backoff_matches_notion_compatible_defaults() {
        let retry = RetryConfig::default();
        assert_eq!(retry.backoff(0), Duration::from_secs(1));
        assert_eq!(retry.backoff(3), Duration::from_secs(8));
        assert_eq!(retry.backoff(99), Duration::from_secs(16));
    }

    #[test]
    fn quota_scopes_have_independent_token_buckets() {
        let orchestrator = NetworkOrchestrator::new(2);
        let left = ConnectorNetworkGate::new(
            orchestrator.clone(),
            ConnectorNetworkConfig::new("left", 1.0, 1.0),
        );
        let right =
            ConnectorNetworkGate::new(orchestrator, ConnectorNetworkConfig::new("right", 1.0, 1.0));

        let _left = left.acquire();
        let right_permit = right.acquire();
        assert!(right_permit.waited() < Duration::from_millis(100));
    }

    #[test]
    fn clients_with_the_same_quota_scope_share_one_token_bucket() {
        let orchestrator = NetworkOrchestrator::new(2);
        let first = ConnectorNetworkGate::new(
            orchestrator.clone(),
            ConnectorNetworkConfig::new("shared", 20.0, 1.0),
        );
        let second = ConnectorNetworkGate::new(
            orchestrator,
            ConnectorNetworkConfig::new("shared", 20.0, 1.0),
        );

        drop(first.acquire());
        let permit = second.acquire();

        assert!(permit.waited() >= Duration::from_millis(35));
    }

    #[test]
    fn thirty_independent_sources_can_use_the_global_capacity_together() {
        let orchestrator = NetworkOrchestrator::new(30);
        let gates = (0..30)
            .map(|index| {
                ConnectorNetworkGate::new(
                    orchestrator.clone(),
                    ConnectorNetworkConfig::new(format!("source-{index}"), 100.0, 1.0),
                )
            })
            .collect::<Vec<_>>();

        let permits = gates
            .iter()
            .map(ConnectorNetworkGate::acquire)
            .collect::<Vec<_>>();

        assert_eq!(permits.len(), 30);
        assert!(
            permits
                .iter()
                .all(|permit| permit.waited() < Duration::from_millis(100))
        );
        assert_eq!(gates[0].status().global_in_flight, 30);
    }

    #[test]
    fn global_limit_applies_without_becoming_a_rate_limit() {
        let orchestrator = NetworkOrchestrator::new(1);
        let first = ConnectorNetworkGate::new(
            orchestrator.clone(),
            ConnectorNetworkConfig::new("first", 100.0, 10.0),
        );
        let second = ConnectorNetworkGate::new(
            orchestrator,
            ConnectorNetworkConfig::new("second", 100.0, 10.0),
        );
        let permit = first.acquire();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let acquired = second.acquire();
            tx.send(acquired.waited()).expect("send wait");
        });
        thread::sleep(Duration::from_millis(30));
        assert!(rx.try_recv().is_err());
        drop(permit);
        assert!(rx.recv_timeout(Duration::from_secs(1)).is_ok());
        handle.join().expect("join waiter");
    }

    #[test]
    fn cooldown_is_local_to_one_scope() {
        let orchestrator = NetworkOrchestrator::new(2);
        let limited = ConnectorNetworkGate::new(
            orchestrator.clone(),
            ConnectorNetworkConfig::new("limited", 100.0, 2.0),
        );
        let healthy = ConnectorNetworkGate::new(
            orchestrator,
            ConnectorNetworkConfig::new("healthy", 100.0, 2.0),
        );
        limited.record_cooldown(Duration::from_millis(60));
        let started = Instant::now();
        let _healthy = healthy.acquire();
        assert!(started.elapsed() < Duration::from_millis(40));
        let limited_permit = limited.acquire();
        assert!(limited_permit.waited() >= Duration::from_millis(40));
    }

    #[test]
    fn cooldown_time_refills_tokens_like_the_existing_notion_limiter() {
        let orchestrator = NetworkOrchestrator::new(1);
        let gate = ConnectorNetworkGate::new(
            orchestrator,
            ConnectorNetworkConfig::new("notion-compatible", 5.0, 1.0),
        );
        gate.record_cooldown(Duration::from_millis(220));

        let permit = gate.acquire();

        assert!(permit.waited() >= Duration::from_millis(180));
        assert!(permit.waited() < Duration::from_millis(320));
    }
}
