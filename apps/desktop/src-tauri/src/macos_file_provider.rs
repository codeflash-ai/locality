// SPDX-License-Identifier: Apache-2.0

use std::ptr::NonNull;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_file_provider::{NSFileProviderDomain, NSFileProviderManager};
use objc2_foundation::{NSArray, NSError, NSString};

const ADD_CALLBACK_TIMEOUT: Duration = Duration::from_secs(15);
const REMOVE_CALLBACK_TIMEOUT: Duration = Duration::from_secs(15);
const DOMAIN_QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const DOMAIN_POLL_TIMEOUT: Duration = Duration::from_secs(30);
const DOMAIN_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DomainActivation {
    Enabled,
    ApprovalRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DomainState {
    user_enabled: bool,
    disconnected: bool,
    hidden: bool,
}

pub(crate) fn register_domain_and_wait(
    app: &tauri::AppHandle,
    identifier: &str,
    display_name: &str,
) -> Result<DomainActivation, String> {
    register_domain_and_wait_opening_after_add(app, identifier, display_name, || Ok(()))
}

pub(crate) fn register_domain_and_wait_opening_after_add<OpenAfterAdd>(
    app: &tauri::AppHandle,
    identifier: &str,
    display_name: &str,
    open_after_add: OpenAfterAdd,
) -> Result<DomainActivation, String>
where
    OpenAfterAdd: FnMut() -> Result<(), String>,
{
    register_domain_and_wait_with_open_after_add(
        |sender| schedule_domain_add(app, identifier, display_name, sender),
        ADD_CALLBACK_TIMEOUT,
        DOMAIN_POLL_TIMEOUT,
        DOMAIN_POLL_INTERVAL,
        open_after_add,
        |remaining| query_domain_state(identifier, remaining.min(DOMAIN_QUERY_TIMEOUT)),
        std::thread::sleep,
        {
            let started = Instant::now();
            move || started.elapsed()
        },
    )
}

pub(crate) fn prepare_approval_retry(identifier: &str, display_name: &str) -> Result<(), String> {
    prepare_approval_retry_with(
        || query_domain_state(identifier, DOMAIN_QUERY_TIMEOUT),
        || remove_domain(identifier, display_name, REMOVE_CALLBACK_TIMEOUT),
    )
}

fn deliver_callback<T>(sender: &SyncSender<T>, value: T) {
    let _ = sender.send(value);
}

fn prepare_approval_retry_with<Query, Remove>(query: Query, remove: Remove) -> Result<(), String>
where
    Query: FnOnce() -> Result<Option<DomainState>, String>,
    Remove: FnOnce() -> Result<(), String>,
{
    match query()? {
        Some(DomainState {
            user_enabled: false,
            ..
        }) => remove().map_err(|error| {
            format!(
                "Could not reset the denied macOS File Provider approval before retrying: {error}"
            )
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
fn register_domain_and_wait_with<Schedule, Query, Sleep, Now>(
    schedule_add: Schedule,
    callback_timeout: Duration,
    poll_timeout: Duration,
    poll_interval: Duration,
    query: Query,
    sleep: Sleep,
    now: Now,
) -> Result<DomainActivation, String>
where
    Schedule: FnOnce(SyncSender<Result<(), String>>) -> Result<(), String>,
    Query: FnMut(Duration) -> Result<Option<DomainState>, String>,
    Sleep: FnMut(Duration),
    Now: FnMut() -> Duration,
{
    register_domain_and_wait_with_open_after_add(
        schedule_add,
        callback_timeout,
        poll_timeout,
        poll_interval,
        || Ok(()),
        query,
        sleep,
        now,
    )
}

fn register_domain_and_wait_with_open_after_add<Schedule, OpenAfterAdd, Query, Sleep, Now>(
    schedule_add: Schedule,
    callback_timeout: Duration,
    poll_timeout: Duration,
    poll_interval: Duration,
    mut open_after_add: OpenAfterAdd,
    mut query: Query,
    mut sleep: Sleep,
    mut now: Now,
) -> Result<DomainActivation, String>
where
    Schedule: FnOnce(SyncSender<Result<(), String>>) -> Result<(), String>,
    OpenAfterAdd: FnMut() -> Result<(), String>,
    Query: FnMut(Duration) -> Result<Option<DomainState>, String>,
    Sleep: FnMut(Duration),
    Now: FnMut() -> Duration,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    schedule_add(sender).map_err(|error| {
        format!("Could not schedule File Provider registration on the main thread: {error}")
    })?;
    match receiver.recv_timeout(callback_timeout) {
        Ok(result) => result?,
        Err(error @ RecvTimeoutError::Timeout) => {
            return Err(format!(
                "File Provider registration callback timed out: {error}"
            ));
        }
        Err(RecvTimeoutError::Disconnected) => {
            return Err(
                "File Provider registration callback disconnected before delivery.".to_string(),
            );
        }
    }

    open_after_add()?;

    let poll_started = now();
    loop {
        let elapsed = now().saturating_sub(poll_started);
        if elapsed >= poll_timeout {
            return Ok(DomainActivation::ApprovalRequired);
        }

        let remaining = poll_timeout - elapsed;
        if let Some(state) = query(remaining)? {
            let (user_enabled, disconnected, hidden) =
                (state.user_enabled, state.disconnected, state.hidden);
            if user_enabled && !disconnected && !hidden {
                return Ok(DomainActivation::Enabled);
            }
        }

        let elapsed = now().saturating_sub(poll_started);
        if elapsed >= poll_timeout {
            return Ok(DomainActivation::ApprovalRequired);
        }
        sleep(poll_interval.min(poll_timeout - elapsed));
    }
}

fn schedule_domain_add(
    app: &tauri::AppHandle,
    identifier: &str,
    display_name: &str,
    completion: SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let identifier = identifier.to_owned();
    let display_name = display_name.to_owned();
    app.run_on_main_thread(move || {
        // SAFETY: The owned strings and completion sender outlive this invocation, and
        // the deployment target supports the File Provider selectors used below.
        unsafe { add_domain(&identifier, &display_name, completion) };
    })
    .map_err(|error| error.to_string())
}

unsafe fn add_domain(
    identifier: &str,
    display_name: &str,
    completion: SyncSender<Result<(), String>>,
) {
    let domain = unsafe { new_domain(identifier, display_name) };
    let completion = RcBlock::new(move |error: *mut NSError| {
        let result = if error.is_null() {
            Ok(())
        } else {
            autoreleasepool(|pool| {
                // SAFETY: File Provider guarantees a non-null NSError pointer remains
                // valid for the duration of its completion callback.
                let error = unsafe { &*error };
                let domain = error.domain();
                let domain = unsafe { domain.to_str(pool) }.to_owned();
                let code = error.code();
                let description = error.localizedDescription();
                let description = unsafe { description.to_str(pool) }.to_owned();
                add_completion_result(&domain, code, &description)
            })
        };
        deliver_callback(&completion, result);
    });

    unsafe { NSFileProviderManager::addDomain_completionHandler(&domain, &completion) };
}

fn remove_domain(identifier: &str, display_name: &str, timeout: Duration) -> Result<(), String> {
    let identifier = identifier.to_owned();
    let display_name = display_name.to_owned();
    let (sender, receiver) = mpsc::sync_channel(1);
    let domain = unsafe { new_domain(&identifier, &display_name) };
    let completion = RcBlock::new(move |error: *mut NSError| {
        let result = if error.is_null() {
            Ok(())
        } else {
            autoreleasepool(|pool| {
                // SAFETY: File Provider guarantees a non-null NSError pointer remains
                // valid for the duration of its completion callback.
                let error = unsafe { &*error };
                let domain = error.domain();
                let domain = unsafe { domain.to_str(pool) }.to_owned();
                let code = error.code();
                let description = error.localizedDescription();
                let description = unsafe { description.to_str(pool) }.to_owned();
                Err(format_framework_error(&domain, code, &description))
            })
        };
        deliver_callback(&sender, result);
    });

    unsafe { NSFileProviderManager::removeDomain_completionHandler(&domain, &completion) };
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(error @ RecvTimeoutError::Timeout) => Err(format!(
            "File Provider domain removal callback timed out: {error}"
        )),
        Err(RecvTimeoutError::Disconnected) => {
            Err("File Provider domain removal callback disconnected before delivery.".to_string())
        }
    }
}

fn query_domain_state(identifier: &str, timeout: Duration) -> Result<Option<DomainState>, String> {
    let identifier = identifier.to_owned();
    let (sender, receiver) = mpsc::sync_channel(1);
    let completion = RcBlock::new(
        move |domains: NonNull<NSArray<NSFileProviderDomain>>, error: *mut NSError| {
            let result = autoreleasepool(|pool| {
                if !error.is_null() {
                    // SAFETY: File Provider guarantees a non-null NSError pointer
                    // remains valid for the duration of its completion callback.
                    let error = unsafe { &*error };
                    let domain = error.domain();
                    let domain = unsafe { domain.to_str(pool) }.to_owned();
                    let code = error.code();
                    let description = error.localizedDescription();
                    let description = unsafe { description.to_str(pool) }.to_owned();
                    return Err(format_framework_error(&domain, code, &description));
                }

                // SAFETY: The generated binding models the domains argument as
                // non-null, and File Provider keeps the NSArray valid for this callback.
                let domains = unsafe { domains.as_ref() };
                let state = domains.iter().find_map(|domain| {
                    let domain_identifier = unsafe { domain.identifier() };
                    if unsafe { domain_identifier.to_str(pool) } != identifier {
                        return None;
                    }

                    Some(DomainState {
                        user_enabled: unsafe { domain.userEnabled() },
                        disconnected: unsafe { domain.isDisconnected() },
                        hidden: unsafe { domain.isHidden() },
                    })
                });
                Ok(state)
            });
            deliver_callback(&sender, result);
        },
    );

    // SAFETY: The copied block owns the requested identifier and sender, so a
    // callback that arrives after the bounded receive does not borrow stack data.
    unsafe { NSFileProviderManager::getDomainsWithCompletionHandler(&completion) };
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(error @ RecvTimeoutError::Timeout) => Err(format!(
            "File Provider domain query callback timed out: {error}"
        )),
        Err(RecvTimeoutError::Disconnected) => {
            Err("File Provider domain query callback disconnected before delivery.".to_string())
        }
    }
}

unsafe fn new_domain(identifier: &str, display_name: &str) -> Retained<NSFileProviderDomain> {
    let identifier = NSString::from_str(identifier);
    let display_name = NSString::from_str(display_name);
    let domain = unsafe {
        NSFileProviderDomain::initWithIdentifier_displayName(
            NSFileProviderDomain::alloc(),
            &identifier,
            &display_name,
        )
    };
    unsafe { domain.setSupportsSyncingTrash(false) };
    domain
}

fn add_completion_result(domain: &str, code: isize, description: &str) -> Result<(), String> {
    if domain == "NSCocoaErrorDomain" && code == objc2_foundation::NSFileWriteFileExistsError {
        Ok(())
    } else {
        Err(format_framework_error(domain, code, description))
    }
}

fn format_framework_error(domain: &str, code: isize, description: &str) -> String {
    format!("{domain} ({code}): {description}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::sync::mpsc::SyncSender;

    fn successful_add(sender: SyncSender<Result<(), String>>) -> Result<(), String> {
        sender.send(Ok(())).expect("send add completion");
        Ok(())
    }

    #[test]
    fn enabled_domain_finishes_without_sleeping() {
        let clock = Cell::new(Duration::ZERO);
        let sleeps = RefCell::new(Vec::new());
        let result = register_domain_and_wait_with(
            successful_add,
            Duration::from_millis(10),
            Duration::from_millis(3),
            Duration::from_millis(1),
            |_| {
                Ok(Some(DomainState {
                    user_enabled: true,
                    disconnected: false,
                    hidden: false,
                }))
            },
            |duration| {
                sleeps.borrow_mut().push(duration);
                clock.set(clock.get() + duration);
            },
            || clock.get(),
        );

        assert_eq!(result, Ok(DomainActivation::Enabled));
        assert!(sleeps.into_inner().is_empty());
    }

    #[test]
    fn polling_waits_for_domain_to_become_enabled() {
        let clock = Cell::new(Duration::ZERO);
        let states = RefCell::new(VecDeque::from([
            None,
            Some(DomainState {
                user_enabled: false,
                disconnected: false,
                hidden: false,
            }),
            Some(DomainState {
                user_enabled: true,
                disconnected: false,
                hidden: false,
            }),
        ]));
        let sleeps = RefCell::new(Vec::new());

        let result = register_domain_and_wait_with(
            successful_add,
            Duration::from_millis(10),
            Duration::from_millis(3),
            Duration::from_millis(1),
            |_| Ok(states.borrow_mut().pop_front().expect("poll state")),
            |duration| {
                sleeps.borrow_mut().push(duration);
                clock.set(clock.get() + duration);
            },
            || clock.get(),
        );

        assert_eq!(result, Ok(DomainActivation::Enabled));
        assert_eq!(sleeps.into_inner(), vec![Duration::from_millis(1); 2]);
    }

    #[test]
    fn approval_retry_opens_domain_after_registration_before_polling() {
        let clock = Cell::new(Duration::ZERO);
        let opened = Cell::new(false);
        let query_saw_open = Cell::new(false);
        let states = RefCell::new(VecDeque::from([
            Some(DomainState {
                user_enabled: false,
                disconnected: false,
                hidden: false,
            }),
            Some(DomainState {
                user_enabled: true,
                disconnected: false,
                hidden: false,
            }),
        ]));

        let result = register_domain_and_wait_with_open_after_add(
            successful_add,
            Duration::from_millis(10),
            Duration::from_millis(3),
            Duration::from_millis(1),
            || {
                opened.set(true);
                Ok(())
            },
            |_| {
                query_saw_open.set(opened.get());
                Ok(states.borrow_mut().pop_front().expect("poll state"))
            },
            |duration| clock.set(clock.get() + duration),
            || clock.get(),
        );

        assert_eq!(result, Ok(DomainActivation::Enabled));
        assert!(opened.get());
        assert!(query_saw_open.get());
    }

    #[test]
    fn disabled_domain_reaches_approval_required_at_the_bound() {
        let clock = Cell::new(Duration::ZERO);
        let result = register_domain_and_wait_with(
            successful_add,
            Duration::from_millis(10),
            Duration::from_millis(2),
            Duration::from_millis(1),
            |_| {
                Ok(Some(DomainState {
                    user_enabled: false,
                    disconnected: false,
                    hidden: false,
                }))
            },
            |duration| clock.set(clock.get() + duration),
            || clock.get(),
        );

        assert_eq!(result, Ok(DomainActivation::ApprovalRequired));
    }

    #[test]
    fn disconnected_domain_reaches_approval_required_even_when_user_enabled() {
        let clock = Cell::new(Duration::ZERO);
        let result = register_domain_and_wait_with(
            successful_add,
            Duration::from_millis(10),
            Duration::from_millis(2),
            Duration::from_millis(1),
            |_| {
                Ok(Some(DomainState {
                    user_enabled: true,
                    disconnected: true,
                    hidden: false,
                }))
            },
            |duration| clock.set(clock.get() + duration),
            || clock.get(),
        );

        assert_eq!(result, Ok(DomainActivation::ApprovalRequired));
    }

    #[test]
    fn hidden_domain_reaches_approval_required_even_when_user_enabled() {
        let clock = Cell::new(Duration::ZERO);
        let result = register_domain_and_wait_with(
            successful_add,
            Duration::from_millis(10),
            Duration::from_millis(2),
            Duration::from_millis(1),
            |_| {
                Ok(Some(DomainState {
                    user_enabled: true,
                    disconnected: false,
                    hidden: true,
                }))
            },
            |duration| clock.set(clock.get() + duration),
            || clock.get(),
        );

        assert_eq!(result, Ok(DomainActivation::ApprovalRequired));
    }

    #[test]
    fn scheduling_failure_is_distinct_from_framework_failure() {
        let clock = Cell::new(Duration::ZERO);
        let result = register_domain_and_wait_with(
            |_| Err("main thread unavailable".to_string()),
            Duration::from_millis(10),
            Duration::from_millis(1),
            Duration::from_millis(1),
            |_| Ok(None),
            |_| {},
            || clock.get(),
        );

        assert_eq!(
            result,
            Err("Could not schedule File Provider registration on the main thread: main thread unavailable".to_string())
        );
    }

    #[test]
    fn framework_failure_is_returned_without_polling() {
        let clock = Cell::new(Duration::ZERO);
        let result = register_domain_and_wait_with(
            |sender| {
                sender
                    .send(add_completion_result(
                        "NSFileProviderErrorDomain",
                        -1,
                        "rejected",
                    ))
                    .expect("send add failure");
                Ok(())
            },
            Duration::from_millis(10),
            Duration::from_millis(1),
            Duration::from_millis(1),
            |_| panic!("framework failure must not poll"),
            |_| {},
            || clock.get(),
        );

        assert_eq!(
            result,
            Err("NSFileProviderErrorDomain (-1): rejected".to_string())
        );
    }

    #[test]
    fn registration_callback_timeout_is_bounded() {
        let clock = Cell::new(Duration::ZERO);
        let pending_sender = RefCell::new(None);
        let result = register_domain_and_wait_with(
            |sender| {
                pending_sender.replace(Some(sender));
                Ok(())
            },
            Duration::from_millis(1),
            Duration::from_millis(1),
            Duration::from_millis(1),
            |_| panic!("timed out registration must not poll"),
            |_| {},
            || clock.get(),
        );

        assert!(
            result
                .expect_err("registration must time out")
                .starts_with("File Provider registration callback timed out:")
        );

        let late_sender = pending_sender
            .take()
            .expect("pending callback sender retained");
        deliver_callback(&late_sender, Ok::<(), String>(()));
    }

    #[test]
    fn state_query_failure_is_returned() {
        let clock = Cell::new(Duration::ZERO);
        let result = register_domain_and_wait_with(
            successful_add,
            Duration::from_millis(10),
            Duration::from_millis(1),
            Duration::from_millis(1),
            |_| Err("domain query failed".to_string()),
            |_| {},
            || clock.get(),
        );

        assert_eq!(result, Err("domain query failed".to_string()));
    }

    #[test]
    fn query_receives_only_the_remaining_poll_deadline() {
        let clock = Cell::new(Duration::ZERO);
        let query_timeouts = RefCell::new(Vec::new());
        let result = register_domain_and_wait_with(
            successful_add,
            Duration::from_millis(10),
            Duration::from_millis(5),
            Duration::from_millis(1),
            |remaining| {
                query_timeouts.borrow_mut().push(remaining);
                clock.set(clock.get() + Duration::from_millis(1));
                Ok(None)
            },
            |duration| clock.set(clock.get() + duration),
            || clock.get(),
        );

        assert_eq!(result, Ok(DomainActivation::ApprovalRequired));
        assert_eq!(
            query_timeouts.into_inner(),
            vec![
                Duration::from_millis(5),
                Duration::from_millis(3),
                Duration::from_millis(1),
            ]
        );
    }

    #[test]
    fn approval_retry_preflight_removes_disabled_domain_before_registration() {
        let removed = Cell::new(false);
        prepare_approval_retry_with(
            || {
                Ok(Some(DomainState {
                    user_enabled: false,
                    disconnected: false,
                    hidden: false,
                }))
            },
            || {
                removed.set(true);
                Ok(())
            },
        )
        .expect("retry preflight succeeds");

        assert!(removed.get());
    }

    #[test]
    fn configured_domain_preserves_non_syncing_trash_semantics() {
        objc2::rc::autoreleasepool(|_| {
            let domain = unsafe { new_domain("loc", "") };
            assert!(!unsafe { domain.supportsSyncingTrash() });
        });
    }

    #[test]
    fn cocoa_file_exists_error_is_an_idempotent_add() {
        assert_eq!(
            add_completion_result(
                "NSCocoaErrorDomain",
                objc2_foundation::NSFileWriteFileExistsError,
                "A file with the same name already exists.",
            ),
            Ok(())
        );
    }
}
