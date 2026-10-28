//! Best-effort anonymous usage reporting. Only explicit binary entrypoints enable
//! a published build; library consumers, examples and development builds stay silent.
mod delivery;
mod events;
mod settings;

use std::{
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

pub use delivery::{
    ProcessLauncher, ProcessSpec, SystemProcessLauncher, capture_process, delegate_process,
    deliver_with,
};
pub use events::{
    CommandContext, Component, Event, ExecutionMode, Lifecycle, LifecycleOutcome, Outcome,
    Rollback, exception_events, lifecycle_events, release_version, safe_operation,
};
pub use settings::{SettingsStatus, SettingsStore};

static RUNTIME_ENABLED: AtomicBool = AtomicBool::new(false);
static BROKER_USER: AtomicU32 = AtomicU32::new(0);
static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
static PANIC_REPORTED: AtomicBool = AtomicBool::new(false);

/// Call only from production binary main functions, never from reusable dispatchers.
pub fn enable_runtime() {
    RUNTIME_ENABLED.store(true, Ordering::Release);
}
pub fn release_enabled() -> bool {
    !cfg!(debug_assertions)
        && !cfg!(test)
        && option_env!("GOFIN_PUBLISHED_RELEASE") == Some("1")
        && option_env!("GOFIN_RELEASE_VERSION").is_some()
}
pub fn production_enabled() -> bool {
    release_enabled() && RUNTIME_ENABLED.load(Ordering::Acquire)
}
pub fn set_broker_user(uid: u32) {
    if uid != 0 {
        BROKER_USER.store(uid, Ordering::Release);
    }
}

pub fn status() -> SettingsStatus {
    match SettingsStore::for_current_account() {
        Ok(store) => store.status(release_enabled()),
        Err(reason) => SettingsStatus {
            enabled: false,
            effective_enabled: false,
            release_enabled: release_enabled(),
            reason,
        },
    }
}
pub fn set_enabled(enabled: bool) -> Result<SettingsStatus, String> {
    SettingsStore::for_current_account()?.set_enabled(enabled, release_enabled())
}
pub fn notice() -> Option<&'static str> {
    if !production_enabled() {
        return None;
    }
    SettingsStore::for_current_account().ok()?.notice(release_version()).then_some("GoFin sends anonymous usage and sanitized error reports. Disable with: gofin analytics disable")
}

pub trait EventSink {
    fn send(&self, events: &[Event]);
}
pub struct ProductionSink;
impl EventSink for ProductionSink {
    fn send(&self, events: &[Event]) {
        report(events);
    }
}

pub fn report(events: &[Event]) {
    if !production_enabled() {
        return;
    }
    if unsafe { libc::geteuid() } == 0 {
        let uid = BROKER_USER.load(Ordering::Acquire);
        if uid != 0 {
            report_as_user(uid, Path::new(delivery::INSTALLED_HELPER), events);
        }
        return;
    }
    if let Ok(store) = SettingsStore::for_current_account() {
        delivery::deliver_with(events, &store, release_enabled(), &SystemProcessLauncher);
    }
}
pub fn report_as_user(uid: u32, helper: &Path, events: &[Event]) {
    if !production_enabled() || uid == 0 {
        return;
    }
    let effective_uid = unsafe { libc::geteuid() };
    if effective_uid == uid {
        report(events);
        return;
    }
    if effective_uid != 0 || !delivery::protected_helper(helper) {
        return;
    }
    if let Some(process) = delegate_process(uid, helper, events) {
        let _ = SystemProcessLauncher.launch(process);
    }
}

fn helper_input() -> Option<Vec<u8>> {
    if unsafe { libc::geteuid() } == 0 || unsafe { libc::geteuid() } != unsafe { libc::getuid() } {
        return None;
    }
    let mut input = Vec::new();
    std::io::stdin()
        .take((delivery::MAX_PAYLOAD + 1) as u64)
        .read_to_end(&mut input)
        .ok()?;
    (input.len() <= delivery::MAX_PAYLOAD).then_some(input)
}
/// Internal only: events are typed and revalidated before their capture encoding.
pub fn run_helper() -> i32 {
    if let Some(input) = helper_input()
        && let Ok(events) = serde_json::from_slice::<Vec<Event>>(&input)
        && events.len() <= 3
    {
        report(&events);
    }
    0
}
/// Wrapper outcome reports do not duplicate the broker's diagnosed exceptions.
pub fn run_command_helper(exit_code: i32) -> i32 {
    if let Some(input) = helper_input()
        && let Ok(context) = serde_json::from_slice::<CommandContext>(&input)
    {
        let outcome = match exit_code {
            0 => Outcome::Succeeded,
            129 | 130 | 143 => Outcome::Canceled,
            _ => Outcome::Failed,
        };
        report(&context.events(outcome, None));
    }
    0
}

pub fn install_panic_hook(component: Component) {
    if !production_enabled() || HOOK_INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |information| {
        // At most one report per process, including reentrant panics while reporting.
        // Never inspect the panic payload, thread name, local values or backtrace.
        if !PANIC_REPORTED.swap(true, Ordering::AcqRel) {
            let (file, line) = information.location().map_or(("external", 0), |location| {
                (location.file(), location.line())
            });
            report(&events::panic_events(component, file, line));
        }
        previous(information);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tests_and_unmarked_builds_cannot_activate_production() {
        enable_runtime();
        assert!(!production_enabled());
        report(&CommandContext::new("doctor", None, None, false).events(Outcome::Succeeded, None));
        install_panic_hook(Component::Cli);
        assert!(!HOOK_INSTALLED.load(Ordering::Acquire));
    }
}
