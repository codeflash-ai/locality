use std::time::Duration;

use locality_core::LocalityResult;
use locality_core::pull::{PullMode, PullSchedulerConfig};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullScheduler {
    pub config: PullSchedulerConfig,
    active_elapsed: Duration,
    cold_elapsed: Duration,
}

impl PullScheduler {
    pub fn new(config: PullSchedulerConfig) -> Self {
        Self {
            active_elapsed: config.active_interval,
            cold_elapsed: config.cold_interval,
            config,
        }
    }

    pub fn tick(&mut self) -> LocalityResult<PullSchedulerTick> {
        self.advance_by(Duration::ZERO)
    }

    pub fn advance_by(&mut self, elapsed: Duration) -> LocalityResult<PullSchedulerTick> {
        validate_interval("active", self.config.active_interval)?;
        validate_interval("cold", self.config.cold_interval)?;

        if self.config.mode == PullMode::Relay {
            return Ok(PullSchedulerTick::default());
        }

        self.active_elapsed = self.active_elapsed.saturating_add(elapsed);
        self.cold_elapsed = self.cold_elapsed.saturating_add(elapsed);

        let poll_active = take_due(&mut self.active_elapsed, self.config.active_interval);
        let poll_cold = take_due(&mut self.cold_elapsed, self.config.cold_interval);

        Ok(PullSchedulerTick {
            poll_active,
            poll_cold,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PullSchedulerTick {
    pub poll_active: bool,
    pub poll_cold: bool,
}

impl PullSchedulerTick {
    pub fn is_idle(&self) -> bool {
        !self.poll_active && !self.poll_cold
    }
}

fn validate_interval(name: &'static str, interval: Duration) -> LocalityResult<()> {
    if interval.is_zero() {
        return Err(locality_core::LocalityError::InvalidState(format!(
            "{name} pull interval must be greater than zero"
        )));
    }

    Ok(())
}

fn take_due(elapsed: &mut Duration, interval: Duration) -> bool {
    if *elapsed < interval {
        return false;
    }

    while *elapsed >= interval {
        *elapsed -= interval;
    }

    true
}
