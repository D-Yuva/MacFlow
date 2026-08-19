use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::MainThreadMarker;
use objc2_application_services::{AXError, AXObserver, AXUIElement};
use objc2_core_foundation::{
    CFRetained, CFRunLoop, CFRunLoopSource, CFString, kCFRunLoopCommonModes,
};

use crate::core::model::{ProcessId, WindowIdentity};

use super::windows::{LiveFocusIdentity, live_focused_window_identity};

const TRACKER_AX_TIMEOUT_SECONDS: f32 = 0.08;
const FOCUSED_WINDOW_CHANGED: &str = "AXFocusedWindowChanged";
const MAIN_WINDOW_CHANGED: &str = "AXMainWindowChanged";
const WINDOW_CREATED: &str = "AXWindowCreated";

/// Whether focus changes arrive from Accessibility notifications or need the
/// controller's existing low-frequency health timer as a safe fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackingMode {
    Idle,
    Observer,
    Polling,
}

/// A single bounded focus transition. `previous` is the last real focused
/// window, even when `current` temporarily becomes `None` for a windowless app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FocusUpdate {
    pub previous: Option<WindowIdentity>,
    pub current: Option<WindowIdentity>,
    pub changed: bool,
}

#[derive(Default)]
struct FocusState {
    current: Option<WindowIdentity>,
    previous: Option<WindowIdentity>,
}

impl FocusState {
    fn apply(&mut self, next: Option<WindowIdentity>) -> FocusUpdate {
        let changed = self.current != next;
        if changed {
            if self.current.is_some() {
                self.previous = self.current;
            }
            self.current = next;
        }
        FocusUpdate {
            previous: self.previous,
            current: self.current,
            changed,
        }
    }
}

struct ObserverSignal {
    dirty: AtomicBool,
}

impl ObserverSignal {
    fn new() -> Self {
        Self {
            dirty: AtomicBool::new(true),
        }
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }
}

unsafe extern "C-unwind" fn focus_observer_callback(
    _observer: NonNull<AXObserver>,
    _element: NonNull<AXUIElement>,
    _notification: NonNull<CFString>,
    refcon: *mut c_void,
) {
    let Some(signal) = NonNull::new(refcon.cast::<ObserverSignal>()) else {
        return;
    };
    // SAFETY: ObserverRegistration is dropped before the Box containing this
    // signal, and dropping the observer removes its run-loop source.
    unsafe { signal.as_ref() }.mark_dirty();
}

struct ObserverRegistration {
    observer: CFRetained<AXObserver>,
    application: CFRetained<AXUIElement>,
    run_loop: CFRetained<CFRunLoop>,
    source: CFRetained<CFRunLoopSource>,
    notifications: [CFRetained<CFString>; 3],
    registered: [bool; 3],
}

impl ObserverRegistration {
    fn new(pid: ProcessId, signal: &ObserverSignal) -> Result<Self, AXError> {
        let application = unsafe { AXUIElement::new_application(pid) };
        let _ = unsafe { application.set_messaging_timeout(TRACKER_AX_TIMEOUT_SECONDS) };
        let mut raw_observer = ptr::null_mut();
        let result = unsafe {
            AXObserver::create(
                pid,
                Some(focus_observer_callback),
                NonNull::from(&mut raw_observer),
            )
        };
        if result != AXError::Success {
            return Err(result);
        }
        let Some(raw_observer) = NonNull::new(raw_observer) else {
            return Err(AXError::Failure);
        };
        let observer = unsafe { CFRetained::from_raw(raw_observer) };
        let notifications = [
            CFString::from_static_str(FOCUSED_WINDOW_CHANGED),
            CFString::from_static_str(MAIN_WINDOW_CHANGED),
            CFString::from_static_str(WINDOW_CREATED),
        ];
        let refcon = ptr::from_ref(signal).cast_mut().cast::<c_void>();
        let mut registered = [false; 3];
        for (index, notification) in notifications.iter().enumerate() {
            let result = unsafe { observer.add_notification(&application, notification, refcon) };
            registered[index] = matches!(
                result,
                AXError::Success | AXError::NotificationAlreadyRegistered
            );
        }
        if !registered.iter().any(|registered| *registered) {
            return Err(AXError::NotificationUnsupported);
        }

        let Some(run_loop) = CFRunLoop::main() else {
            return Err(AXError::Failure);
        };
        let source = unsafe { observer.run_loop_source() };
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
        Ok(Self {
            observer,
            application,
            run_loop,
            source,
            notifications,
            registered,
        })
    }
}

impl Drop for ObserverRegistration {
    fn drop(&mut self) {
        self.run_loop
            .remove_source(Some(&self.source), unsafe { kCFRunLoopCommonModes });
        for (index, notification) in self.notifications.iter().enumerate() {
            if self.registered[index] {
                let _ = unsafe {
                    self.observer
                        .remove_notification(&self.application, notification)
                };
            }
        }
    }
}

/// Tracks only the current frontmost process, so observer memory remains O(1)
/// regardless of how many applications or windows are open.
///
/// The AX callback does no discovery or allocation; it only flips one atomic
/// dirty bit. The controller can call `sample_if_needed` from its main-thread
/// action drain. When an app rejects AX notifications, `TrackingMode::Polling`
/// tells the controller to call `sample` from its existing health timer.
pub struct LiveFocusTracker {
    // Registration must be released before its callback refcon below.
    registration: Option<ObserverRegistration>,
    signal: Box<ObserverSignal>,
    tracked_pid: Option<ProcessId>,
    mode: TrackingMode,
    state: FocusState,
    // AXObserver's source and refcon are installed on the main run loop. This
    // marker prevents the owner (and therefore Drop) from moving to a worker.
    _main_thread: MainThreadMarker,
}

impl LiveFocusTracker {
    #[must_use]
    pub fn new(mtm: MainThreadMarker) -> Self {
        Self {
            registration: None,
            signal: Box::new(ObserverSignal::new()),
            tracked_pid: None,
            mode: TrackingMode::Idle,
            state: FocusState::default(),
            _main_thread: mtm,
        }
    }

    /// Rebinds the sole observer when NSWorkspace reports a frontmost-process
    /// change. Unsupported or untrusted AX endpoints fail into polling mode.
    pub fn track_application(&mut self, pid: Option<ProcessId>) -> TrackingMode {
        if self.tracked_pid == pid {
            return self.mode;
        }
        self.registration = None;
        self.tracked_pid = pid;
        self.signal.mark_dirty();
        let Some(pid) = pid else {
            self.mode = TrackingMode::Idle;
            return self.mode;
        };
        match ObserverRegistration::new(pid, &self.signal) {
            Ok(registration) => {
                self.registration = Some(registration);
                self.mode = TrackingMode::Observer;
            }
            Err(_) => self.mode = TrackingMode::Polling,
        }
        self.mode
    }

    /// Forces the next `sample_if_needed` call to refresh. This is the fallback
    /// hook for workspace activation/window-close notifications outside AX.
    pub fn mark_dirty(&self) {
        self.signal.mark_dirty();
    }

    #[must_use]
    pub const fn mode(&self) -> TrackingMode {
        self.mode
    }

    #[must_use]
    pub const fn current(&self) -> Option<WindowIdentity> {
        self.state.current
    }

    #[must_use]
    pub const fn previous(&self) -> Option<WindowIdentity> {
        self.state.previous
    }

    /// Samples only after an observer/fallback event. Polling mode intentionally
    /// reports work on each caller-controlled health tick.
    pub fn sample_if_needed(&mut self) -> Option<FocusUpdate> {
        if self.mode != TrackingMode::Polling && !self.signal.take_dirty() {
            return None;
        }
        Some(self.sample())
    }

    /// Performs one bounded current-process sample. Snapshot failure preserves
    /// the last known identity rather than inventing a focus transition.
    #[must_use]
    pub fn sample(&mut self) -> FocusUpdate {
        let _ = self.signal.take_dirty();
        let Some(pid) = self.tracked_pid else {
            return self.apply_sample(None);
        };
        match live_focused_window_identity(pid) {
            LiveFocusIdentity::Focused(identity) => self.apply_sample(Some(identity)),
            LiveFocusIdentity::NoWindow => self.apply_sample(None),
            LiveFocusIdentity::SnapshotUnavailable => FocusUpdate {
                previous: self.state.previous,
                current: self.state.current,
                changed: false,
            },
        }
    }

    fn apply_sample(&mut self, next: Option<WindowIdentity>) -> FocusUpdate {
        self.state.apply(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_history_preserves_the_last_real_window() {
        let mut tracker = FocusState::default();
        let first = WindowIdentity::new(1, 10);
        let second = WindowIdentity::new(2, 20);

        let update = tracker.apply(Some(first));
        assert!(update.changed);
        assert_eq!(update.previous, None);
        assert_eq!(update.current, Some(first));

        let update = tracker.apply(Some(second));
        assert!(update.changed);
        assert_eq!(update.previous, Some(first));
        assert_eq!(update.current, Some(second));

        let update = tracker.apply(None);
        assert!(update.changed);
        assert_eq!(update.previous, Some(second));
        assert_eq!(update.current, None);
    }

    #[test]
    fn repeated_samples_do_not_reorder_history() {
        let mut tracker = FocusState::default();
        let first = WindowIdentity::new(1, 10);
        let second = WindowIdentity::new(1, 11);
        tracker.apply(Some(first));
        tracker.apply(Some(second));
        for _ in 0..100_000 {
            let update = tracker.apply(Some(second));
            assert!(!update.changed);
            assert_eq!(update.previous, Some(first));
            assert_eq!(update.current, Some(second));
        }
    }

    #[test]
    fn dirty_signal_coalesces_unbounded_event_bursts() {
        let signal = ObserverSignal::new();
        assert!(signal.take_dirty());
        for _ in 0..250_000 {
            signal.mark_dirty();
        }
        assert!(signal.take_dirty());
        assert!(!signal.take_dirty());
    }

    #[test]
    fn windowless_to_real_transition_keeps_the_prior_real_alternate() {
        let mut tracker = FocusState::default();
        let first = WindowIdentity::new(1, 10);
        let second = WindowIdentity::new(2, 20);
        tracker.apply(Some(first));
        tracker.apply(None);
        let update = tracker.apply(Some(second));
        assert_eq!(update.previous, Some(first));
        assert_eq!(update.current, Some(second));
    }
}
