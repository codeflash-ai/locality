use std::time::Duration;

use locality_core::pull::{PullMode, PullSchedulerConfig};
use localityd::scheduler::PullScheduler;

#[test]
fn polling_scheduler_runs_active_and_cold_polls_on_first_tick() {
    let mut scheduler = PullScheduler::new(PullSchedulerConfig::default());

    let tick = scheduler.tick().expect("initial tick");

    assert!(tick.poll_active);
    assert!(tick.poll_cold);
}

#[test]
fn polling_scheduler_respects_active_and_cold_intervals() {
    let mut scheduler = PullScheduler::new(PullSchedulerConfig {
        active_interval: Duration::from_secs(15),
        cold_interval: Duration::from_secs(300),
        ..PullSchedulerConfig::default()
    });
    scheduler.tick().expect("initial tick");

    let idle = scheduler
        .advance_by(Duration::from_secs(14))
        .expect("not due");
    assert!(idle.is_idle());

    let active = scheduler
        .advance_by(Duration::from_secs(1))
        .expect("active due");
    assert!(active.poll_active);
    assert!(!active.poll_cold);

    let both = scheduler
        .advance_by(Duration::from_secs(285))
        .expect("cold due");
    assert!(both.poll_active);
    assert!(both.poll_cold);
}

#[test]
fn relay_scheduler_does_not_poll() {
    let mut scheduler = PullScheduler::new(PullSchedulerConfig {
        mode: PullMode::Relay,
        ..PullSchedulerConfig::default()
    });

    let tick = scheduler.tick().expect("relay tick");

    assert!(tick.is_idle());
}

#[test]
fn zero_intervals_are_rejected() {
    let mut scheduler = PullScheduler::new(PullSchedulerConfig {
        active_interval: Duration::ZERO,
        ..PullSchedulerConfig::default()
    });

    let error = scheduler.tick().expect_err("zero interval");

    assert_eq!(
        error.to_string(),
        "invalid state: active pull interval must be greater than zero"
    );
}
