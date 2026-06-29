//! Pull scheduler configuration.
//!
//! The daemon owns timers and connector calls. The core owns the policy shape so
//! direct polling and future relay-backed feeds can share the same sync model.

use std::time::Duration;

use crate::hydration::HydrationPolicy;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PullMode {
    Polling,
    Relay,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullSchedulerConfig {
    pub mode: PullMode,
    pub active_interval: Duration,
    pub cold_interval: Duration,
    pub hydration_policy: HydrationPolicy,
}

impl Default for PullSchedulerConfig {
    fn default() -> Self {
        Self {
            mode: PullMode::Polling,
            active_interval: Duration::from_secs(15),
            cold_interval: Duration::from_secs(300),
            hydration_policy: HydrationPolicy::default(),
        }
    }
}
