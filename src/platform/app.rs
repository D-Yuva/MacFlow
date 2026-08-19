use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSEvent,
    NSEventModifierFlags, NSMenu, NSMenuItem, NSPasteboard, NSPasteboardTypeString,
    NSRunningApplication, NSScreen, NSSquareStatusItemLength, NSStatusBar, NSStatusItem,
    NSWorkspace,
};
use objc2_application_services::AXIsProcessTrusted;
use objc2_core_graphics::{CGImage, CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
use objc2_foundation::{
    MainThreadMarker, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSProcessInfo, NSRect,
    NSSize, NSString, NSTimer, NSURL, NSUserDefaults,
};
use objc2_service_management::{SMAppService, SMAppServiceStatus};

use crate::core::hotkey::{Action, Modifiers, Shortcut};
use crate::core::model::{
    Bounds, WindowId, WindowIdentity, WindowKey, WindowOrder, pin_window_to_front,
    preferred_alternate_index, sort_by_stable_mru_and_z_order,
};
use crate::core::session::{Generation, Phase, RecentWindowHistory, SwitcherSession};

use super::capture::{
    CaptureMode, CapturePipeline, CaptureSize, CaptureTarget, MAX_TARGETS_PER_BATCH,
};
use super::hotkey::HotkeyTap;
use super::overlay::{CurrentWindow, GridNavigation, Overlay};
use super::window_tracker::LiveFocusTracker;
use super::windows::{
    AppSeed, DiscoveryResult, DisplayGeometry, ExactQuickSwitchResult, ManagedWindow,
    cancel_pending_focus, discover_windows, focus_window, live_window_snapshot,
    quick_switch_window_exact,
};

const MAX_CACHED_WINDOWS: usize = 192;
const MAX_RUNNING_APPS: usize = 128;
const DISCOVERY_STACK_BYTES: usize = 384 * 1024;
const ACTION_MAILBOX_CAPACITY: usize = 64;
const ACTION_DRAIN_BATCH: usize = 16;
const DELIVERY_RETRY_DELAY: Duration = Duration::from_millis(1);
const HOTKEY_RECOVERY_INTERVAL_SECONDS: u8 = 1;
const HOTKEY_HEALTHY_INTERVAL_SECONDS: u8 = 10;
const BUNDLE_IDENTIFIER: &str = "io.github.dyuva.macflow";
const SHORTCUT_DEFAULTS_KEY: &str = "MacFlowShortcut";
const REVEAL_DELAY_DEFAULTS_KEY: &str = "MacFlowRevealDelayMs";
const QUICK_SWITCH_DEFAULTS_KEY: &str = "MacFlowQuickSwitch";
const THUMBNAIL_MODE_DEFAULTS_KEY: &str = "MacFlowThumbnailMode";
const DISPLAY_TARGET_DEFAULTS_KEY: &str = "MacFlowDisplayTarget";
const UPDATE_URL: &str = "https://github.com/D-Yuva/MacFlow/releases/latest";
const FRESH_DISCOVERY_MAX_AGE: Duration = Duration::from_millis(250);
const DEFAULT_REVEAL_DELAY_MS: u64 = 90;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ThumbnailPreference {
    Off,
    Selected,
    Visible,
    #[default]
    Fresh,
}

impl ThumbnailPreference {
    const ALL: [Self; 4] = [Self::Off, Self::Selected, Self::Visible, Self::Fresh];

    const fn code(self) -> isize {
        match self {
            Self::Off => 0,
            Self::Selected => 1,
            Self::Visible => 2,
            Self::Fresh => 3,
        }
    }

    const fn from_code(code: isize) -> Self {
        match code {
            0 => Self::Off,
            1 => Self::Selected,
            2 => Self::Visible,
            _ => Self::Fresh,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off (icons only)",
            Self::Selected => "Selected window only",
            Self::Visible => "Visible windows",
            Self::Fresh => "Maximum freshness",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DisplayTarget {
    #[default]
    FocusedWindow,
    Mouse,
    Main,
}

impl DisplayTarget {
    const ALL: [Self; 3] = [Self::FocusedWindow, Self::Mouse, Self::Main];

    const fn code(self) -> isize {
        match self {
            Self::FocusedWindow => 0,
            Self::Mouse => 1,
            Self::Main => 2,
        }
    }

    const fn from_code(code: isize) -> Self {
        match code {
            1 => Self::Mouse,
            2 => Self::Main,
            _ => Self::FocusedWindow,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::FocusedWindow => "Focused window display",
            Self::Mouse => "Mouse display",
            Self::Main => "Main display",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Preferences {
    reveal_delay_ms: u64,
    quick_switch: bool,
    thumbnails: ThumbnailPreference,
    display_target: DisplayTarget,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            reveal_delay_ms: DEFAULT_REVEAL_DELAY_MS,
            quick_switch: true,
            thumbnails: ThumbnailPreference::Fresh,
            display_target: DisplayTarget::FocusedWindow,
        }
    }
}

impl Preferences {
    fn reveal_delay(self) -> Duration {
        Duration::from_millis(self.reveal_delay_ms.clamp(0, 500))
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RuntimeMetrics {
    discovery_runs: u64,
    last_discovery_ms: u64,
    last_overlay_ms: u64,
    last_switch_ms: u64,
    last_window_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ThumbnailWorkingSet {
    indexes: [usize; MAX_TARGETS_PER_BATCH],
    len: usize,
}

impl ThumbnailWorkingSet {
    fn from_overlay(overlay: &Overlay) -> Self {
        let mut result = Self {
            indexes: [0; MAX_TARGETS_PER_BATCH],
            len: 0,
        };
        result.len = overlay.thumbnail_indexes(&mut result.indexes);
        result
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.indexes[..self.len].iter().copied()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnqueueResult {
    Queued,
    QueuedAndSchedule,
    Dropped,
    Closed,
}

/// A fixed-size FIFO for event-tap handoff.
///
/// Human input cannot normally fill this queue: a main-queue drain consumes a
/// batch after the event-tap callback returns. The overflow policy is still
/// deliberate because synthetic input can be effectively unbounded. A final
/// Confirm/Cancel is never discarded in favour of navigation or pre-warming;
/// the oldest expendable action is removed and the terminal action remains at
/// the tail, preserving the order of every retained entry.
struct ActionMailbox {
    slots: [Option<Action>; ACTION_MAILBOX_CAPACITY],
    head: usize,
    len: usize,
    drain_scheduled: bool,
    draining: bool,
    closed: bool,
}

impl Default for ActionMailbox {
    fn default() -> Self {
        Self {
            slots: [None; ACTION_MAILBOX_CAPACITY],
            head: 0,
            len: 0,
            drain_scheduled: false,
            draining: false,
            closed: false,
        }
    }
}

impl ActionMailbox {
    fn reopen(&mut self) {
        *self = Self::default();
    }

    fn close(&mut self) {
        self.slots.fill(None);
        self.head = 0;
        self.len = 0;
        self.drain_scheduled = false;
        self.draining = false;
        self.closed = true;
    }

    fn enqueue(&mut self, action: Action) -> EnqueueResult {
        if self.closed {
            return EnqueueResult::Closed;
        }

        if self.len == ACTION_MAILBOX_CAPACITY {
            if !is_terminal_action(action) {
                return EnqueueResult::Dropped;
            }

            let removable = (0..self.len)
                .find(|offset| is_expendable_action(self.get(*offset)))
                // A stream made entirely of Show/terminal actions is not
                // produced by HotkeyState, but retaining the newest terminal
                // is the safest bounded fallback for corrupted/synthetic input.
                .unwrap_or(0);
            self.remove(removable);
        }

        let tail = (self.head + self.len) % ACTION_MAILBOX_CAPACITY;
        self.slots[tail] = Some(action);
        self.len += 1;

        if !self.drain_scheduled && !self.draining {
            self.drain_scheduled = true;
            EnqueueResult::QueuedAndSchedule
        } else {
            EnqueueResult::Queued
        }
    }

    fn begin_drain(&mut self) -> bool {
        if self.closed || self.draining || !self.drain_scheduled {
            return false;
        }
        self.drain_scheduled = false;
        self.draining = true;
        true
    }

    fn finish_drain(&mut self) -> bool {
        self.draining = false;
        if self.closed || self.len == 0 || self.drain_scheduled {
            return false;
        }
        self.drain_scheduled = true;
        true
    }

    fn pop_front(&mut self) -> Option<Action> {
        if self.len == 0 {
            return None;
        }
        let action = self.slots[self.head].take();
        self.head = (self.head + 1) % ACTION_MAILBOX_CAPACITY;
        self.len -= 1;
        action
    }

    fn get(&self, offset: usize) -> Action {
        debug_assert!(offset < self.len);
        self.slots[(self.head + offset) % ACTION_MAILBOX_CAPACITY].expect("occupied mailbox slot")
    }

    fn remove(&mut self, offset: usize) {
        debug_assert!(offset < self.len);
        for current in offset..self.len - 1 {
            let next = (self.head + current + 1) % ACTION_MAILBOX_CAPACITY;
            let destination = (self.head + current) % ACTION_MAILBOX_CAPACITY;
            self.slots[destination] = self.slots[next].take();
        }
        let tail = (self.head + self.len - 1) % ACTION_MAILBOX_CAPACITY;
        self.slots[tail] = None;
        self.len -= 1;
    }
}

const fn is_terminal_action(action: Action) -> bool {
    matches!(action, Action::Confirm | Action::Cancel)
}

const fn is_expendable_action(action: Action) -> bool {
    matches!(
        action,
        Action::PreWarm
            | Action::Next
            | Action::Previous
            | Action::Left
            | Action::Right
            | Action::Up
            | Action::Down
            | Action::First
            | Action::Last
            | Action::ReenableTap
    )
}

const fn gesture_activity_update(action: Action) -> Option<bool> {
    match action {
        Action::Show | Action::ShowPrevious => Some(true),
        Action::Confirm | Action::Cancel | Action::ReenableTap => Some(false),
        _ => None,
    }
}

struct Delivery {
    alive: AtomicBool,
    gesture_active: AtomicBool,
    controller: AtomicPtr<AppController>,
    controller_busy: AtomicBool,
    actions: Mutex<ActionMailbox>,
}

impl Delivery {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            alive: AtomicBool::new(false),
            gesture_active: AtomicBool::new(false),
            controller: AtomicPtr::new(ptr::null_mut()),
            controller_busy: AtomicBool::new(false),
            actions: Mutex::new(ActionMailbox::default()),
        })
    }

    fn bind(&self, controller: *mut AppController) {
        lock(&self.actions).reopen();
        self.gesture_active.store(false, Ordering::Release);
        self.controller_busy.store(false, Ordering::Release);
        self.controller.store(controller, Ordering::Release);
        self.alive.store(true, Ordering::Release);
    }

    fn clear(&self) {
        self.alive.store(false, Ordering::Release);
        self.gesture_active.store(false, Ordering::Release);
        self.controller.store(ptr::null_mut(), Ordering::Release);
        lock(&self.actions).close();
    }

    /// Runs controller work exclusively on the main thread. If AppKit pumps a
    /// nested run loop while another controller call is active, the work is
    /// re-dispatched as a single delayed closure instead of aliasing `&mut` or
    /// growing an in-memory retry queue.
    fn on_main<F>(self: &Arc<Self>, callback: F)
    where
        F: FnOnce(&mut AppController) + Send + 'static,
    {
        if !self.alive.load(Ordering::Acquire) {
            return;
        }

        match self.try_controller() {
            ControllerAccess::Ready(mut lease) => callback(lease.controller()),
            ControllerAccess::Retry => {
                let delivery = Arc::clone(self);
                let when =
                    DispatchTime::try_from(DELIVERY_RETRY_DELAY).unwrap_or(DispatchTime::NOW);
                let _ = DispatchQueue::main().after(when, move || {
                    delivery.on_main(callback);
                });
            }
            ControllerAccess::Closed => {}
        }
    }

    /// The event-tap callback calls only this method. It performs no heap
    /// allocation unless it transitions the mailbox from idle to scheduled.
    fn enqueue_action(self: &Arc<Self>, action: Action) {
        if !self.alive.load(Ordering::Acquire) {
            return;
        }
        if let Some(active) = gesture_activity_update(action) {
            self.gesture_active.store(active, Ordering::Release);
        }
        let result = lock(&self.actions).enqueue(action);
        if result == EnqueueResult::QueuedAndSchedule {
            Self::dispatch_action_drain(Arc::clone(self), false);
        }
    }

    fn dispatch_action_drain(delivery: Arc<Self>, delayed: bool) {
        if delayed {
            let when = DispatchTime::try_from(DELIVERY_RETRY_DELAY).unwrap_or(DispatchTime::NOW);
            let _ = DispatchQueue::main().after(when, move || delivery.drain_actions());
        } else {
            DispatchQueue::main().exec_async(move || delivery.drain_actions());
        }
    }

    fn drain_actions(self: &Arc<Self>) {
        {
            let mut actions = lock(&self.actions);
            if !actions.begin_drain() {
                return;
            }
        }

        let mut retry = false;
        match self.try_controller() {
            ControllerAccess::Ready(mut lease) => {
                for _ in 0..ACTION_DRAIN_BATCH {
                    if !self.alive.load(Ordering::Acquire) {
                        break;
                    }
                    let action = lock(&self.actions).pop_front();
                    let Some(action) = action else {
                        break;
                    };
                    lease.controller().handle_action(action);
                }
            }
            ControllerAccess::Retry => retry = true,
            ControllerAccess::Closed => {}
        }

        let schedule_next = lock(&self.actions).finish_drain();
        if schedule_next {
            Self::dispatch_action_drain(Arc::clone(self), retry);
        }
    }

    fn try_controller(&self) -> ControllerAccess<'_> {
        if !self.alive.load(Ordering::Acquire) {
            return ControllerAccess::Closed;
        }
        if MainThreadMarker::new().is_none() {
            return ControllerAccess::Retry;
        }
        if self
            .controller_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return ControllerAccess::Retry;
        }

        let controller = self.controller.load(Ordering::Acquire);
        if !self.alive.load(Ordering::Acquire) || controller.is_null() {
            self.controller_busy.store(false, Ordering::Release);
            return ControllerAccess::Closed;
        }

        ControllerAccess::Ready(ControllerLease {
            delivery: self,
            controller,
        })
    }
}

enum ControllerAccess<'a> {
    Ready(ControllerLease<'a>),
    Retry,
    Closed,
}

struct ControllerLease<'a> {
    delivery: &'a Delivery,
    controller: *mut AppController,
}

impl ControllerLease<'_> {
    fn controller(&mut self) -> &mut AppController {
        // SAFETY: Delivery binds the stable allocation behind the delegate's
        // Box and clears it before that Box is released. Only the main thread
        // can obtain a lease, and controller_busy excludes nested leases.
        unsafe { &mut *self.controller }
    }
}

impl Drop for ControllerLease<'_> {
    fn drop(&mut self) {
        self.delivery
            .controller_busy
            .store(false, Ordering::Release);
    }
}

struct DiscoveryRequest {
    sequence: u64,
    apps: Vec<AppSeed>,
    displays: Vec<DisplayGeometry>,
}

#[derive(Default)]
struct DiscoveryQueueState {
    pending: Option<DiscoveryRequest>,
    stopping: bool,
}

struct DiscoveryShared {
    state: Mutex<DiscoveryQueueState>,
    wake: Condvar,
    delivery: Arc<Delivery>,
}

impl DiscoveryShared {
    fn should_cancel(&self) -> bool {
        let state = lock(&self.state);
        state.stopping || state.pending.is_some()
    }
}

struct DiscoveryWorker {
    shared: Arc<DiscoveryShared>,
    thread: Option<JoinHandle<()>>,
}

impl DiscoveryWorker {
    fn new(delivery: Arc<Delivery>) -> std::io::Result<Self> {
        let shared = Arc::new(DiscoveryShared {
            state: Mutex::new(DiscoveryQueueState::default()),
            wake: Condvar::new(),
            delivery,
        });
        let worker_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("macflow-discovery".into())
            .stack_size(DISCOVERY_STACK_BYTES)
            .spawn(move || discovery_loop(worker_shared))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Replaces the sole pending request. If discovery is already running,
    /// the worker finishes that bounded pass and then observes only the newest
    /// requested application/screen snapshot.
    fn submit(&self, request: DiscoveryRequest) {
        let mut state = lock(&self.shared.state);
        if state.stopping {
            return;
        }
        state.pending = Some(request);
        self.shared.wake.notify_one();
    }
}

impl Drop for DiscoveryWorker {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.shared.state);
            state.stopping = true;
            state.pending = None;
            self.shared.wake.notify_one();
        }
        // Discovery holds only Arc-owned state and publishes through a Delivery
        // that is closed during teardown. Detaching avoids beachballing the
        // AppKit main thread if a third-party process stalls one final AX call.
        let _ = self.thread.take();
    }
}

fn discovery_loop(shared: Arc<DiscoveryShared>) {
    loop {
        let request = {
            let mut state = lock(&shared.state);
            while state.pending.is_none() && !state.stopping {
                state = wait(&shared.wake, state);
            }
            if state.stopping {
                return;
            }
            state.pending.take()
        };
        let Some(request) = request else {
            continue;
        };
        let sequence = request.sequence;
        let result = autoreleasepool(|_| {
            discover_windows(&request.apps, &request.displays, || shared.should_cancel())
        });
        let Some(result) = result else {
            continue;
        };
        if shared.should_cancel() {
            continue;
        }
        let delivery = Arc::clone(&shared.delivery);
        DispatchQueue::main().exec_async(move || {
            delivery.on_main(move |controller| controller.discovery_finished(sequence, result));
        });
    }
}

struct AppController {
    overlay: Overlay,
    // The Overlay field is dropped first and clears its callback before this
    // stable callback context is released.
    selection_delivery: Box<Weak<Delivery>>,
    shortcut: Shortcut,
    preferences: Preferences,
    session: SwitcherSession,
    recent: RecentWindowHistory,
    recent_scratch: Vec<WindowIdentity>,
    focus_tracker: LiveFocusTracker,
    cached_windows: Vec<ManagedWindow>,
    active_windows: Vec<ManagedWindow>,
    cached_z_order: Vec<WindowOrder>,
    pending_show: Option<Generation>,
    origin_pid: Option<i32>,
    origin_window_id: Option<u32>,
    delivery: Arc<Delivery>,
    discovery: Option<DiscoveryWorker>,
    captures: Option<CapturePipeline>,
    last_ready_sequence: u64,
    discovery_sequence: u64,
    discovery_in_flight: bool,
    last_discovery_completed: Option<Instant>,
    confirm_when_ready: bool,
    refresh_required_for_confirm: bool,
    overlay_reveal_sequence: u64,
    overlay_reveal_due: bool,
    overlay_visible: bool,
    pending_forward: u64,
    pending_backward: u64,
    pending_reverse: bool,
    discovery_started: Option<(u64, Instant)>,
    show_started: Option<Instant>,
    metrics: RuntimeMetrics,
}

impl AppController {
    fn new(mtm: MainThreadMarker, shortcut: Shortcut, preferences: Preferences) -> Self {
        let delivery = Delivery::new();
        let discovery = DiscoveryWorker::new(Arc::clone(&delivery)).ok();
        let captures = CapturePipeline::new().ok();
        if let Some(captures) = &captures {
            captures.set_mode(capture_mode(preferences.thumbnails));
            let weak_delivery = Arc::downgrade(&delivery);
            captures.set_update_handler(move || {
                if let Some(delivery) = weak_delivery.upgrade() {
                    delivery.on_main(AppController::capture_updated);
                }
            });
        }
        let selection_delivery = Box::new(Arc::downgrade(&delivery));
        Self {
            overlay: Overlay::new(mtm),
            selection_delivery,
            shortcut,
            preferences,
            session: SwitcherSession::default(),
            recent: RecentWindowHistory::new(40),
            recent_scratch: Vec::with_capacity(40),
            focus_tracker: LiveFocusTracker::new(mtm),
            cached_windows: Vec::new(),
            active_windows: Vec::new(),
            cached_z_order: Vec::new(),
            pending_show: None,
            origin_pid: None,
            origin_window_id: None,
            delivery,
            discovery,
            captures,
            last_ready_sequence: 0,
            discovery_sequence: 0,
            discovery_in_flight: false,
            last_discovery_completed: None,
            confirm_when_ready: false,
            refresh_required_for_confirm: false,
            overlay_reveal_sequence: 0,
            overlay_reveal_due: false,
            overlay_visible: false,
            pending_forward: 0,
            pending_backward: 0,
            pending_reverse: false,
            discovery_started: None,
            show_started: None,
            metrics: RuntimeMetrics::default(),
        }
    }

    fn bind(&mut self) {
        let pointer = self as *mut Self;
        self.delivery.bind(pointer);
        let delivery = (&*self.selection_delivery as *const Weak<Delivery>)
            .cast_mut()
            .cast::<c_void>();
        self.overlay.set_callback(selection_changed, delivery);
        self.overlay
            .set_activation_callback(selection_activated, delivery);
        self.overlay
            .set_viewport_callback(viewport_changed, delivery);
    }

    fn handle_action(&mut self, action: Action) {
        match action {
            Action::PreWarm => {
                self.refresh_live_focus_and_order();
                let _ = self.request_refresh();
            }
            Action::Show => self.show(false),
            Action::ShowPrevious => self.show(true),
            Action::Next => self.select_next(),
            Action::Previous => self.select_previous(),
            Action::Left => self.navigate(GridNavigation::Left),
            Action::Right => self.navigate(GridNavigation::Right),
            Action::Up => self.navigate(GridNavigation::Up),
            Action::Down => self.navigate(GridNavigation::Down),
            Action::First => self.navigate(GridNavigation::Home),
            Action::Last => self.navigate(GridNavigation::End),
            Action::Confirm => self.hide(true),
            Action::Cancel => self.hide(false),
            // The tap re-enables itself before handing this diagnostic action
            // to the app, so no re-entrant access to HotkeyTap is necessary.
            Action::ReenableTap => {}
        }
    }

    fn request_refresh(&mut self) -> Option<u64> {
        let Some(discovery) = &self.discovery else {
            return None;
        };
        self.discovery_sequence = self.discovery_sequence.wrapping_add(1);
        let sequence = self.discovery_sequence;
        self.discovery_started = Some((sequence, Instant::now()));
        discovery.submit(DiscoveryRequest {
            sequence,
            apps: collect_apps(),
            displays: collect_displays(),
        });
        self.discovery_in_flight = true;
        Some(sequence)
    }

    fn refresh_live_focus_and_order(&mut self) {
        let frontmost_pid = NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|app| app.processIdentifier());
        self.origin_pid = frontmost_pid;
        self.focus_tracker.track_application(frontmost_pid);
        // Sample on every gesture. Finder and browser tab changes can mint a
        // new WindowServer ID without emitting AXFocusedWindowChanged.
        let update = self.focus_tracker.sample();
        if update.changed {
            if let Some(previous) = update.previous {
                self.recent.record(previous);
            }
            if let Some(current) = update.current {
                self.recent.record(current);
            }
        }
        self.origin_window_id = self
            .focus_tracker
            .current()
            .filter(|identity| Some(identity.pid) == frontmost_pid)
            .map(|identity| identity.window_id);

        if let Some(snapshot) = live_window_snapshot() {
            self.recent.prune_live(snapshot.identities());
            self.cached_z_order = snapshot
                .identities()
                .iter()
                .enumerate()
                .map(|(z_index, identity)| WindowOrder {
                    window_id: identity.window_id,
                    z_index,
                })
                .collect();
        }
    }

    fn discovery_finished(&mut self, sequence: u64, mut result: DiscoveryResult) {
        // A modifier pre-warm can be in flight when Show submits a newer cold
        // request. Never let that older snapshot consume pending_show.
        if sequence != self.discovery_sequence {
            return;
        }
        let was_visible = self.session.phase() == Phase::Visible;
        let selected_key = if was_visible {
            self.active_windows
                .get(self.session.selected_index())
                .map(|window| window.metadata.key.clone())
        } else {
            None
        };
        self.discovery_in_flight = false;
        if let Some((started_sequence, started)) = self.discovery_started.take()
            && started_sequence == sequence
        {
            self.metrics.discovery_runs = self.metrics.discovery_runs.saturating_add(1);
            self.metrics.last_discovery_ms = duration_millis(started.elapsed());
        }
        self.last_discovery_completed = Some(Instant::now());
        result.windows.truncate(MAX_CACHED_WINDOWS);
        if let Some(captures) = &self.captures {
            let changed = changed_thumbnail_keys(&self.cached_windows, &result.windows);
            captures.invalidate_many(&changed);
        }
        self.cached_windows = result.windows;
        self.metrics.last_window_count = self.cached_windows.len();
        self.cached_z_order = result.z_order;
        self.sort_cached();
        self.reset_capture_generation();

        if let Some(generation) = self.pending_show.take() {
            let base_selection = if self.pending_reverse {
                self.cached_windows.len().saturating_sub(1)
            } else {
                preferred_alternate_index(
                    &self.cached_windows,
                    |window| window.metadata.key.window_id,
                    self.origin_window_id,
                )
            };
            let selected = selection_after_pending_navigation(
                base_selection,
                self.cached_windows.len(),
                self.pending_forward,
                self.pending_backward,
            );
            self.pending_forward = 0;
            self.pending_backward = 0;
            self.pending_reverse = false;
            if !self
                .session
                .present_at(generation, self.cached_windows.len(), selected)
            {
                self.confirm_when_ready = false;
                return;
            }
            self.active_windows.clone_from(&self.cached_windows);
            if self.confirm_when_ready {
                self.confirm_when_ready = false;
                self.hide(true);
            } else {
                self.present_active_if_due();
            }
            return;
        }

        if was_visible && self.refresh_required_for_confirm {
            let selected = selected_key
                .as_ref()
                .and_then(|selected| {
                    self.cached_windows
                        .iter()
                        .position(|window| window.metadata.key == *selected)
                })
                .unwrap_or_else(|| {
                    preferred_alternate_index(
                        &self.cached_windows,
                        |window| window.metadata.key.window_id,
                        self.origin_window_id,
                    )
                });
            self.refresh_required_for_confirm = false;
            if !self
                .session
                .refresh_visible(self.cached_windows.len(), selected)
            {
                self.confirm_when_ready = false;
                self.hide(false);
                return;
            }
            self.active_windows.clone_from(&self.cached_windows);
            if self.confirm_when_ready {
                self.confirm_when_ready = false;
                self.hide(true);
            } else if self.overlay_visible {
                self.overlay
                    .update(&self.active_windows, self.session.selected_index());
                self.refresh_thumbnail_views(false);
                self.queue_active_thumbnails();
            } else {
                self.present_active_if_due();
            }
        }
    }

    fn schedule_delayed_overlay(&mut self) {
        self.overlay_reveal_sequence = self.overlay_reveal_sequence.wrapping_add(1);
        self.overlay_reveal_due = false;
        if self.overlay_visible {
            self.overlay.hide();
            self.overlay.clear();
        }
        self.overlay_visible = false;
        if !self.preferences.quick_switch {
            self.overlay_reveal_due = true;
            return;
        }
        let sequence = self.overlay_reveal_sequence;
        let delivery = Arc::clone(&self.delivery);
        let when =
            DispatchTime::try_from(self.preferences.reveal_delay()).unwrap_or(DispatchTime::NOW);
        let _ = DispatchQueue::main().after(when, move || {
            delivery.on_main(move |controller| controller.overlay_delay_elapsed(sequence));
        });
    }

    fn overlay_delay_elapsed(&mut self, sequence: u64) {
        if !overlay_delay_is_current(sequence, self.overlay_reveal_sequence, self.session.phase()) {
            return;
        }
        if !self.reveal_input_is_active() {
            self.suppress_delayed_overlay();
            return;
        }
        self.overlay_reveal_due = true;
        self.present_active_if_due();
    }

    fn present_active_if_due(&mut self) {
        if self.overlay_reveal_due
            && !self.overlay_visible
            && self.session.phase() == Phase::Visible
        {
            if self.reveal_input_is_active() {
                self.present_active();
            } else {
                self.suppress_delayed_overlay();
            }
        }
    }

    fn reveal_input_is_active(&self) -> bool {
        self.delivery.gesture_active.load(Ordering::Acquire)
            && shortcut_is_physically_held(self.shortcut)
    }

    fn suppress_delayed_overlay(&mut self) {
        self.overlay_reveal_sequence = self.overlay_reveal_sequence.wrapping_add(1);
        self.overlay_reveal_due = false;
        if self.overlay_visible {
            self.overlay.hide();
            self.overlay.clear();
        }
        self.overlay_visible = false;
    }

    fn show(&mut self, reverse: bool) {
        cancel_pending_focus();
        self.show_started = Some(Instant::now());
        self.confirm_when_ready = false;
        self.refresh_required_for_confirm = false;
        self.pending_show = None;
        self.pending_forward = 0;
        self.pending_backward = 0;
        self.pending_reverse = reverse;
        let generation = self.session.begin_show();
        self.schedule_delayed_overlay();
        self.refresh_live_focus_and_order();
        let cache_contains_live_origin = origin_is_represented(
            &self.cached_windows,
            |window| window.metadata.key.window_id,
            self.origin_window_id,
        );

        let discovery_age = self
            .last_discovery_completed
            .map(|completed| completed.elapsed());
        if self.cached_windows.is_empty() {
            self.pending_show = Some(generation);
            if self.request_refresh().is_none() {
                self.pending_show = None;
                if let Some(hide_generation) = self.session.begin_hide() {
                    let _ = self.session.finish_hide(hide_generation);
                }
            }
            return;
        }

        // Finder and browser tab changes can mint a new CG window ID that is
        // absent from the cached presentation. Start an authoritative refresh,
        // but let the existing cards render at the reveal deadline instead of
        // making the chooser wait for the full AX discovery budget. Confirmation
        // remains gated below, so this visual-only snapshot can never focus a
        // stale or wrong window.
        let identity_refresh_started = if cache_contains_live_origin {
            false
        } else if self.request_refresh().is_some() {
            true
        } else {
            if let Some(hide_generation) = self.session.begin_hide() {
                let _ = self.session.finish_hide(hide_generation);
            }
            return;
        };

        self.sort_cached();
        let selected = if reverse {
            self.cached_windows.len().saturating_sub(1)
        } else {
            preferred_alternate_index(
                &self.cached_windows,
                |window| window.metadata.key.window_id,
                self.origin_window_id,
            )
        };
        if !self
            .session
            .present_at(generation, self.cached_windows.len(), selected)
        {
            return;
        }
        self.active_windows.clone_from(&self.cached_windows);
        if identity_refresh_started {
            self.refresh_required_for_confirm = true;
        } else if !discovery_is_fresh(true, discovery_age) {
            self.refresh_required_for_confirm = true;
            if !self.discovery_in_flight && self.request_refresh().is_none() {
                self.refresh_required_for_confirm = false;
            }
        }
        self.present_active_if_due();
    }

    fn present_active(&mut self) {
        let origin_bounds = self.origin_window_id.and_then(|window_id| {
            self.active_windows
                .iter()
                .find(|window| window.metadata.key.window_id == Some(window_id))
                .map(|window| window.metadata.bounds)
        });
        let frame = chooser_screen_frame(self.preferences.display_target, origin_bounds);
        let selected = self.session.selected_index();
        self.overlay.set_current_window(
            self.origin_pid
                .zip(self.origin_window_id)
                .map(|(pid, window_id)| CurrentWindow { pid, window_id }),
        );
        let shortcut = self.shortcut;
        let delivery = &self.delivery;
        if !self
            .overlay
            .show(frame, &self.active_windows, selected, || {
                delivery.gesture_active.load(Ordering::Acquire)
                    && shortcut_is_physically_held(shortcut)
            })
        {
            self.overlay.clear();
            self.suppress_delayed_overlay();
            return;
        }
        self.overlay_visible = true;
        if let Some(started) = self.show_started {
            self.metrics.last_overlay_ms = duration_millis(started.elapsed());
        }
        self.refresh_thumbnail_views(false);
        self.queue_active_thumbnails();
    }

    fn select_next(&mut self) {
        if self.session.phase() == Phase::AwaitingWindows {
            self.pending_forward = self.pending_forward.wrapping_add(1);
            return;
        }
        self.session.select_next();
        self.selection_did_change();
    }

    fn select_previous(&mut self) {
        if self.session.phase() == Phase::AwaitingWindows {
            self.pending_backward = self.pending_backward.wrapping_add(1);
            return;
        }
        self.session.select_previous();
        self.selection_did_change();
    }

    fn select_index(&mut self, index: usize) {
        if self.session.select_index(index) {
            self.selection_did_change();
        }
    }

    fn navigate(&mut self, direction: GridNavigation) {
        if self.session.phase() == Phase::AwaitingWindows {
            match direction {
                GridNavigation::Left | GridNavigation::Up | GridNavigation::Home => {
                    self.pending_backward = self.pending_backward.wrapping_add(1);
                }
                GridNavigation::Right | GridNavigation::Down | GridNavigation::End => {
                    self.pending_forward = self.pending_forward.wrapping_add(1);
                }
            }
            return;
        }
        if !self.overlay_visible {
            match direction {
                GridNavigation::Left | GridNavigation::Up | GridNavigation::Home => {
                    self.select_previous();
                }
                GridNavigation::Right | GridNavigation::Down | GridNavigation::End => {
                    self.select_next();
                }
            }
            return;
        }
        if let Some(index) = self.overlay.navigate(direction) {
            self.select_index(index);
        }
    }

    fn selection_did_change(&mut self) {
        if self.session.phase() != Phase::Visible || !self.overlay_visible {
            return;
        }
        self.overlay.set_selection(self.session.selected_index());
        self.refresh_thumbnail_views(false);
        self.queue_active_thumbnails();
    }

    fn viewport_did_change(&mut self) {
        if self.session.phase() != Phase::Visible {
            return;
        }
        self.refresh_thumbnail_views(false);
        self.queue_active_thumbnails();
    }

    fn hide(&mut self, confirm: bool) {
        cancel_pending_focus();
        let selected = confirm
            .then(|| {
                self.active_windows
                    .get(self.session.selected_index())
                    .cloned()
            })
            .flatten();
        // Modifier release must cancel the chooser immediately even when an
        // identity refresh is still required before the headless switch can
        // safely commit. This prevents a delayed panel flash after release.
        self.suppress_delayed_overlay();
        let confirmation_would_wait = confirm
            && ((self.session.phase() == Phase::AwaitingWindows && self.pending_show.is_some())
                || (self.session.phase() == Phase::Visible
                    && self.refresh_required_for_confirm
                    && self.discovery_in_flight));
        let exact_quick_focus = if confirmation_would_wait {
            selected.as_ref().is_some_and(|window| {
                match quick_switch_window_exact(&window.metadata.key) {
                    ExactQuickSwitchResult::Focused => true,
                    ExactQuickSwitchResult::WindowClosed => {
                        if let Some(identity) = window.metadata.key.identity() {
                            self.recent.remove(identity);
                        }
                        false
                    }
                    ExactQuickSwitchResult::InvalidKey
                    | ExactQuickSwitchResult::SnapshotUnavailable
                    | ExactQuickSwitchResult::RequiresFullFocus => false,
                }
            })
        } else {
            false
        };
        if confirmation_would_wait && !exact_quick_focus {
            self.confirm_when_ready = true;
            return;
        }
        self.confirm_when_ready = false;
        self.refresh_required_for_confirm = false;
        self.pending_show = None;
        self.pending_forward = 0;
        self.pending_backward = 0;
        self.pending_reverse = false;
        let origin_identity = self
            .origin_pid
            .zip(self.origin_window_id)
            .map(|(pid, window_id)| WindowIdentity::new(pid, window_id));
        self.origin_window_id = None;
        self.origin_pid = None;
        let hide_generation = self.session.begin_hide();
        self.overlay.hide();
        self.overlay.clear();
        self.overlay.set_current_window(None);
        self.active_windows.clear();
        if let Some(generation) = hide_generation {
            let _ = self.session.finish_hide(generation);
        }

        if let Some(window) = selected {
            let destination = window.metadata.key.identity();
            if !exact_quick_focus {
                let focus_started = Instant::now();
                focus_window(&window);
                self.metrics.last_switch_ms = duration_millis(focus_started.elapsed());
            } else if let Some(started) = self.show_started {
                self.metrics.last_switch_ms = duration_millis(started.elapsed());
            }
            self.recent.record_switch(origin_identity, destination);
        }
    }

    fn sort_cached(&mut self) {
        self.recent_scratch.clear();
        let (first, second) = self.recent.as_slices();
        self.recent_scratch.extend_from_slice(first);
        self.recent_scratch.extend_from_slice(second);
        sort_by_stable_mru_and_z_order(
            &mut self.cached_windows,
            |window| window.metadata.key.identity(),
            &self.recent_scratch,
            &self.cached_z_order,
        );
        let _ = pin_window_to_front(
            &mut self.cached_windows,
            |window| window.metadata.key.window_id,
            self.origin_window_id,
        );
    }

    fn diagnostics_report(&self) -> String {
        let capture = self
            .captures
            .as_ref()
            .map(|pipeline| (pipeline.stats(), pipeline.freshness_policy()));
        let os = NSProcessInfo::processInfo().operatingSystemVersionString();
        let capture_summary = capture.map_or_else(
            || "unavailable".to_owned(),
            |(stats, freshness)| {
                format!(
                    "mode={:?}, {} entries, {} / {} bytes, {} pending, busy={}, selected TTL={} ms, visible TTL={} ms",
                    stats.mode,
                    stats.cache_entries,
                    stats.cache_bytes,
                    stats.cache_budget_bytes,
                    stats.pending_targets,
                    stats.capture_in_flight,
                    duration_millis(freshness.selected_max_age),
                    duration_millis(freshness.visible_max_age),
                )
            },
        );
        format!(
            concat!(
                "MacFlow {}\n",
                "macOS: {}\n",
                "Architecture: {}\n",
                "Shortcut: {}\n",
                "Reveal delay: {} ms\n",
                "Quick switch: {}\n",
                "Thumbnail mode: {}\n",
                "Chooser display: {}\n",
                "Accessibility: {}\n",
                "Screen Recording: {}\n",
                "Cached windows: {}\n",
                "Discovery runs: {}\n",
                "Last discovery: {} ms\n",
                "Last chooser reveal: {} ms\n",
                "Last focus handoff: {} ms\n",
                "Capture: {}\n",
                "Private cross-Space focus: {}\n"
            ),
            env!("CARGO_PKG_VERSION"),
            os,
            std::env::consts::ARCH,
            self.shortcut.label(),
            self.preferences.reveal_delay_ms,
            self.preferences.quick_switch,
            self.preferences.thumbnails.label(),
            self.preferences.display_target.label(),
            unsafe { AXIsProcessTrusted() },
            CGPreflightScreenCaptureAccess(),
            self.metrics.last_window_count,
            self.metrics.discovery_runs,
            self.metrics.last_discovery_ms,
            self.metrics.last_overlay_ms,
            self.metrics.last_switch_ms,
            capture_summary,
            if cfg!(feature = "app-store") {
                "disabled"
            } else {
                "guarded"
            }
        )
    }

    fn reset_capture_generation(&mut self) {
        let Some(captures) = &self.captures else {
            return;
        };
        let live_keys = self
            .cached_windows
            .iter()
            .map(|window| window.metadata.key.clone())
            .collect::<Vec<_>>();
        let _ = captures.begin_generation(&live_keys);
    }

    fn apply_capture_preference(&mut self) {
        let Some(captures) = &self.captures else {
            return;
        };
        captures.set_mode(capture_mode(self.preferences.thumbnails));
        if self.preferences.thumbnails == ThumbnailPreference::Off {
            captures.invalidate_all();
            self.overlay.retain_thumbnails(&[]);
            return;
        }
        if self.overlay_visible {
            self.refresh_thumbnail_views(false);
            self.queue_active_thumbnails();
        }
    }

    fn queue_active_thumbnails(&mut self) -> bool {
        let Some(captures) = &self.captures else {
            return false;
        };
        let generation = captures.generation();
        let mut targets = Vec::with_capacity(MAX_TARGETS_PER_BATCH);
        let working_set = ThumbnailWorkingSet::from_overlay(&self.overlay);
        for index in working_set.iter() {
            let window = &self.active_windows[index];
            targets.push(thumbnail_capture_target(window));
        }
        captures.submit(generation, &targets).accepted != 0
    }

    fn refresh_thumbnail_views(&mut self, replace_existing: bool) {
        let working_set = ThumbnailWorkingSet::from_overlay(&self.overlay);
        let mut retained_ids = [0_u32; MAX_TARGETS_PER_BATCH];
        let mut retained_len = 0;
        for index in working_set.iter() {
            if let Some(window_id) = self.active_windows[index].metadata.key.window_id {
                retained_ids[retained_len] = window_id;
                retained_len += 1;
            }
        }
        if self.preferences.thumbnails == ThumbnailPreference::Off {
            self.overlay.retain_thumbnails(&[]);
            return;
        }
        self.overlay
            .retain_thumbnails(&retained_ids[..retained_len]);

        let Some(captures) = &self.captures else {
            return;
        };
        for index in working_set.iter() {
            let Some(window) = self.active_windows.get(index) else {
                continue;
            };
            let Some(window_id) = window.metadata.key.window_id else {
                continue;
            };
            if !replace_existing && self.overlay.has_thumbnail(window_id) {
                continue;
            }
            let Some(image) = captures.get(
                &window.metadata.key,
                thumbnail_capture_size(window.metadata.bounds),
            ) else {
                continue;
            };
            let size = NSSize::new(
                CGImage::width(Some(&image)) as f64,
                CGImage::height(Some(&image)) as f64,
            );
            let image = objc2_app_kit::NSImage::initWithCGImage_size(
                objc2_app_kit::NSImage::alloc(),
                &image,
                size,
            );
            let _ = self.overlay.set_thumbnail(window_id, image);
        }
    }

    fn capture_updated(&mut self) {
        let Some(captures) = &self.captures else {
            return;
        };
        let stats = captures.acknowledge_update();
        if self.session.phase() != Phase::Visible || !self.overlay_visible {
            self.last_ready_sequence = stats.ready_sequence;
            return;
        }
        let ready_changed = stats.ready_sequence != self.last_ready_sequence;
        self.last_ready_sequence = stats.ready_sequence;
        if ready_changed {
            self.refresh_thumbnail_views(true);
        } else {
            self.refresh_thumbnail_views(false);
        }
        if !stats.capture_in_flight && stats.pending_targets == 0 {
            self.queue_active_thumbnails();
        }
    }
}

const fn capture_mode(preference: ThumbnailPreference) -> CaptureMode {
    match preference {
        ThumbnailPreference::Off => CaptureMode::Off,
        ThumbnailPreference::Selected => CaptureMode::Selected,
        ThumbnailPreference::Visible => CaptureMode::Visible,
        ThumbnailPreference::Fresh => CaptureMode::Fresh,
    }
}

fn changed_thumbnail_keys(previous: &[ManagedWindow], current: &[ManagedWindow]) -> Vec<WindowKey> {
    current
        .iter()
        .filter_map(|window| {
            let old = previous
                .iter()
                .find(|candidate| candidate.metadata.key == window.metadata.key)?;
            (old.metadata.title != window.metadata.title
                || old.metadata.bounds != window.metadata.bounds
                || old.metadata.minimized != window.metadata.minimized)
                .then(|| window.metadata.key.clone())
        })
        .collect()
}

fn thumbnail_capture_size(bounds: Bounds) -> CaptureSize {
    CaptureSize::preserving_aspect_ratio(bounds.width, bounds.height)
}

fn discovery_is_fresh(has_windows: bool, age: Option<Duration>) -> bool {
    has_windows && age.is_some_and(|age| age <= FRESH_DISCOVERY_MAX_AGE)
}

fn origin_is_represented<T>(
    windows: &[T],
    window_id: impl Fn(&T) -> Option<WindowId>,
    origin_window_id: Option<WindowId>,
) -> bool {
    origin_window_id.is_none_or(|origin| {
        windows
            .iter()
            .any(|window| window_id(window) == Some(origin))
    })
}

fn selection_after_pending_navigation(
    base: usize,
    window_count: usize,
    forward: u64,
    backward: u64,
) -> usize {
    if window_count == 0 {
        return 0;
    }
    let count = window_count as u64;
    let offset = ((forward % count) + count - (backward % count)) % count;
    (base % window_count + offset as usize) % window_count
}

fn overlay_delay_is_current(callback_sequence: u64, current_sequence: u64, phase: Phase) -> bool {
    callback_sequence == current_sequence
        && matches!(phase, Phase::AwaitingWindows | Phase::Visible)
}

fn shortcut_is_physically_held(shortcut: Shortcut) -> bool {
    reveal_input_matches_flags(true, shortcut, NSEvent::modifierFlags_class())
}

fn shortcut_matches_flags(shortcut: Shortcut, flags: NSEventModifierFlags) -> bool {
    shortcut.matches_modifiers(Modifiers {
        control: flags.contains(NSEventModifierFlags::Control),
        option: flags.contains(NSEventModifierFlags::Option),
        command: flags.contains(NSEventModifierFlags::Command),
        shift: flags.contains(NSEventModifierFlags::Shift),
    })
}

fn reveal_input_matches_flags(
    gesture_active: bool,
    shortcut: Shortcut,
    flags: NSEventModifierFlags,
) -> bool {
    gesture_active && shortcut_matches_flags(shortcut, flags)
}

fn thumbnail_capture_target(window: &ManagedWindow) -> CaptureTarget {
    CaptureTarget::new(
        window.metadata.key.clone(),
        thumbnail_capture_size(window.metadata.bounds),
    )
}

impl Drop for AppController {
    fn drop(&mut self) {
        cancel_pending_focus();
        self.delivery.clear();
        self.overlay.hide();
    }
}

unsafe extern "C" fn selection_changed(context: *mut c_void, index: usize) {
    if context.is_null() {
        return;
    }
    // SAFETY: AppController stores this Weak in a stable Box. Its Overlay field
    // is dropped first and clears this callback before the Box is released.
    let delivery = unsafe { &*context.cast::<Weak<Delivery>>() }.upgrade();
    if let Some(delivery) = delivery {
        delivery.on_main(move |controller| controller.select_index(index));
    }
}

unsafe extern "C" fn selection_activated(context: *mut c_void, index: usize) {
    if context.is_null() {
        return;
    }
    // SAFETY: This uses the same stable Weak context as selection_changed.
    let delivery = unsafe { &*context.cast::<Weak<Delivery>>() }.upgrade();
    if let Some(delivery) = delivery {
        delivery.on_main(move |controller| {
            controller.select_index(index);
            controller.hide(true);
        });
    }
}

unsafe extern "C" fn viewport_changed(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: This is the same stable Weak context used by selection_changed;
    // Overlay clears both callbacks before AppController releases it.
    let delivery = unsafe { &*context.cast::<Weak<Delivery>>() }.upgrade();
    if let Some(delivery) = delivery {
        delivery.on_main(AppController::viewport_did_change);
    }
}

struct AppDelegateIvars {
    hotkey: RefCell<Option<HotkeyTap>>,
    shortcut: Cell<Shortcut>,
    preferences: Cell<Preferences>,
    paused: Cell<bool>,
    controller: RefCell<Option<Box<AppController>>>,
    // Kept separate from `controller` so nested AppKit callbacks never create
    // an `&AppController` while Delivery holds its exclusive controller lease.
    delivery: RefCell<Option<Arc<Delivery>>>,
    status_item: RefCell<Option<Retained<NSStatusItem>>>,
    permission_timer: RefCell<Option<Retained<NSTimer>>>,
    permission_interval_seconds: Cell<u8>,
    permission_snapshot: Cell<Option<(bool, bool)>>,
}

impl Default for AppDelegateIvars {
    fn default() -> Self {
        Self {
            hotkey: RefCell::new(None),
            shortcut: Cell::new(Shortcut::default()),
            preferences: Cell::new(Preferences::default()),
            paused: Cell::new(false),
            controller: RefCell::new(None),
            delivery: RefCell::new(None),
            status_item: RefCell::new(None),
            permission_timer: RefCell::new(None),
            permission_interval_seconds: Cell::new(0),
            permission_snapshot: Cell::new(None),
        }
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. Delegate and all its
    // retained AppKit state are main-thread-only.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = AppDelegateIvars]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            // An untrusted development bundle must not evict the working,
            // Accessibility-authorized installed copy. The signed installed
            // replacement is trusted and can safely retire stale duplicates.
            // SAFETY: This system query has no preconditions.
            if unsafe { AXIsProcessTrusted() } {
                terminate_other_instances();
            }
            let shortcut = load_shortcut();
            let preferences = load_preferences();
            self.ivars().shortcut.set(shortcut);
            self.ivars().preferences.set(preferences);
            let mut controller = Box::new(AppController::new(
                self.mtm(),
                shortcut,
                preferences,
            ));
            controller.bind();
            self.ivars()
                .delivery
                .replace(Some(Arc::clone(&controller.delivery)));
            self.ivars().controller.replace(Some(controller));

            self.setup_status_item();
            // Thumbnail capture is optional and privacy-sensitive. Ask only
            // after the user explicitly selects the menu action instead of
            // presenting a Screen Recording prompt on first launch.
            self.ensure_hotkey();
            self.start_permission_timer();
            let delivery = self.ivars().delivery.borrow().clone();
            if let Some(delivery) = delivery {
                delivery.on_main(|controller| {
                    let _ = controller.request_refresh();
                });
            }
            self.rebuild_menu();
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            if let Some(timer) = self.ivars().permission_timer.borrow_mut().take() {
                timer.invalidate();
            }
            self.ivars().permission_interval_seconds.set(0);
            // Stop new event-tap callbacks first. The controller allocation is
            // intentionally kept in the delegate until `app.run()` returns:
            // termination can arrive from a nested AppKit run loop while a
            // controller lease is active, and freeing it here would invalidate
            // that lease. Delivery is closed immediately, so queued callbacks
            // become harmless no-ops.
            self.ivars().hotkey.borrow_mut().take();
            let delivery = self.ivars().delivery.borrow_mut().take();
            if let Some(delivery) = delivery {
                delivery.clear();
            }
        }
    }

    impl AppDelegate {
        #[unsafe(method(permissionPoll:))]
        fn permission_poll(&self, _timer: &NSTimer) {
            let trusted = unsafe { AXIsProcessTrusted() };
            let mut should_hide = false;
            {
                let mut hotkey = self.ivars().hotkey.borrow_mut();
                if !trusted {
                    should_hide = hotkey.take().is_some();
                } else if let Some(tap) = hotkey.as_ref()
                    && (tap.handler_panicked() || (!tap.is_enabled() && !tap.reenable()))
                {
                    hotkey.take();
                    should_hide = true;
                }
            }
            self.ensure_hotkey();
            if should_hide {
                let delivery = self.ivars().delivery.borrow().clone();
                if let Some(delivery) = delivery {
                    delivery.gesture_active.store(false, Ordering::Release);
                    delivery.on_main(|controller| controller.hide(false));
                }
            }
            let snapshot = (trusted, CGPreflightScreenCaptureAccess());
            if self.ivars().permission_snapshot.get() != Some(snapshot) {
                self.rebuild_menu();
            }
            self.start_permission_timer();
        }

        #[unsafe(method(promptAccessibility:))]
        fn prompt_accessibility(&self, _sender: Option<&AnyObject>) {
            open_settings(
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
            );
        }

        #[unsafe(method(promptScreenRecording:))]
        fn prompt_screen_recording(&self, _sender: Option<&AnyObject>) {
            if !CGRequestScreenCaptureAccess() {
                open_settings(
                    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
                );
            }
            self.rebuild_menu();
        }

        #[unsafe(method(openScreenRecordingSettings:))]
        fn open_screen_recording_settings(&self, _sender: Option<&AnyObject>) {
            open_settings(
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
            );
        }

        #[unsafe(method(shortcutControlOptionTab:))]
        fn shortcut_control_option_tab(&self, _sender: Option<&AnyObject>) {
            self.set_shortcut(Shortcut::ControlOptionTab);
        }

        #[unsafe(method(shortcutCommandTab:))]
        fn shortcut_command_tab(&self, _sender: Option<&AnyObject>) {
            self.set_shortcut(Shortcut::CommandTab);
        }

        #[unsafe(method(shortcutControlCommandTab:))]
        fn shortcut_control_command_tab(&self, _sender: Option<&AnyObject>) {
            self.set_shortcut(Shortcut::ControlCommandTab);
        }

        #[unsafe(method(shortcutControlOptionSpace:))]
        fn shortcut_control_option_space(&self, _sender: Option<&AnyObject>) {
            self.set_shortcut(Shortcut::ControlOptionSpace);
        }

        #[unsafe(method(shortcutCommandOptionSpace:))]
        fn shortcut_command_option_space(&self, _sender: Option<&AnyObject>) {
            self.set_shortcut(Shortcut::CommandOptionSpace);
        }

        #[unsafe(method(setRevealDelay:))]
        fn set_reveal_delay(&self, sender: Option<&AnyObject>) {
            let Some(milliseconds) = sender_tag(sender).and_then(|tag| u64::try_from(tag).ok())
            else {
                return;
            };
            let mut preferences = self.ivars().preferences.get();
            preferences.reveal_delay_ms = milliseconds.clamp(0, 500);
            self.set_preferences(preferences);
        }

        #[unsafe(method(toggleQuickSwitch:))]
        fn toggle_quick_switch(&self, _sender: Option<&AnyObject>) {
            let mut preferences = self.ivars().preferences.get();
            preferences.quick_switch = !preferences.quick_switch;
            self.set_preferences(preferences);
        }

        #[unsafe(method(setThumbnailMode:))]
        fn set_thumbnail_mode(&self, sender: Option<&AnyObject>) {
            let Some(code) = sender_tag(sender) else {
                return;
            };
            let mut preferences = self.ivars().preferences.get();
            preferences.thumbnails = ThumbnailPreference::from_code(code);
            self.set_preferences(preferences);
        }

        #[unsafe(method(setDisplayTarget:))]
        fn set_display_target(&self, sender: Option<&AnyObject>) {
            let Some(code) = sender_tag(sender) else {
                return;
            };
            let mut preferences = self.ivars().preferences.get();
            preferences.display_target = DisplayTarget::from_code(code);
            self.set_preferences(preferences);
        }

        #[unsafe(method(togglePause:))]
        fn toggle_pause(&self, _sender: Option<&AnyObject>) {
            let paused = !self.ivars().paused.get();
            self.ivars().paused.set(paused);
            if paused {
                self.ivars().hotkey.borrow_mut().take();
                if let Some(delivery) = self.ivars().delivery.borrow().clone() {
                    delivery.gesture_active.store(false, Ordering::Release);
                    delivery.on_main(|controller| controller.hide(false));
                }
            } else {
                self.ensure_hotkey();
            }
            self.start_permission_timer();
            self.rebuild_menu();
        }

        #[unsafe(method(toggleLaunchAtLogin:))]
        fn toggle_launch_at_login(&self, _sender: Option<&AnyObject>) {
            let service = unsafe { SMAppService::mainAppService() };
            let status = unsafe { service.status() };
            let result = if status == SMAppServiceStatus::Enabled {
                unsafe { service.unregisterAndReturnError() }
            } else {
                unsafe { service.registerAndReturnError() }
            };
            if let Err(error) = result {
                eprintln!("MacFlow: Launch at Login update failed: {error}");
            }
            if unsafe { service.status() } == SMAppServiceStatus::RequiresApproval {
                unsafe { SMAppService::openSystemSettingsLoginItems() };
            }
            self.rebuild_menu();
        }

        #[unsafe(method(checkForUpdates:))]
        fn check_for_updates(&self, _sender: Option<&AnyObject>) {
            open_settings(UPDATE_URL);
        }

        #[unsafe(method(copyDiagnostics:))]
        fn copy_diagnostics(&self, _sender: Option<&AnyObject>) {
            let delivery = self.ivars().delivery.borrow().clone();
            if let Some(delivery) = delivery {
                delivery.on_main(|controller| {
                    copy_to_clipboard(&controller.diagnostics_report());
                });
            }
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AppDelegateIvars::default());
        // SAFETY: NSObject's init signature is correct for this fresh subclass.
        unsafe { msg_send![super(this), init] }
    }

    fn ensure_hotkey(&self) {
        if self.ivars().paused.get() || self.ivars().hotkey.borrow().is_some() {
            return;
        }
        // SAFETY: This system query has no preconditions.
        if !unsafe { AXIsProcessTrusted() } {
            self.start_permission_timer();
            return;
        }
        let delivery = self.ivars().delivery.borrow().clone();
        let Some(delivery) = delivery else {
            return;
        };
        match HotkeyTap::install(self.ivars().shortcut.get(), move |action| {
            // The synchronous event-tap callback only records the action. UI,
            // discovery and focus work starts asynchronously after the system
            // callback has returned.
            delivery.enqueue_action(action);
        }) {
            Ok(tap) => {
                self.ivars().hotkey.replace(Some(tap));
                self.configure_permission_timer(HOTKEY_HEALTHY_INTERVAL_SECONDS);
            }
            Err(error) => {
                eprintln!("MacFlow: {error}");
                self.configure_permission_timer(HOTKEY_RECOVERY_INTERVAL_SECONDS);
            }
        }
    }

    fn set_preferences(&self, preferences: Preferences) {
        self.ivars().preferences.set(preferences);
        save_preferences(preferences);
        let delivery = self.ivars().delivery.borrow().clone();
        if let Some(delivery) = delivery {
            delivery.on_main(move |controller| {
                controller.preferences = preferences;
                controller.apply_capture_preference();
                if controller.overlay_visible {
                    controller.hide(false);
                }
            });
        }
        self.rebuild_menu();
    }

    fn set_shortcut(&self, shortcut: Shortcut) {
        self.ivars().shortcut.set(shortcut);
        // Persist and push the selection even when the user re-selects the
        // checked menu item. This makes the menu a reliable way to repair a
        // listener whose in-memory chord was left stale by an older build.
        save_shortcut(shortcut);
        if let Some(tap) = self.ivars().hotkey.borrow().as_ref() {
            tap.set_shortcut(shortcut);
        } else {
            self.ensure_hotkey();
        }
        let delivery = self.ivars().delivery.borrow().clone();
        if let Some(delivery) = delivery {
            delivery.gesture_active.store(false, Ordering::Release);
            delivery.on_main(move |controller| {
                controller.shortcut = shortcut;
                controller.hide(false);
            });
        }
        self.rebuild_menu();
    }

    fn start_permission_timer(&self) {
        let healthy = self.ivars().paused.get()
            || (unsafe { AXIsProcessTrusted() }
                && self
                    .ivars()
                    .hotkey
                    .borrow()
                    .as_ref()
                    .is_some_and(|tap| tap.is_enabled() && !tap.handler_panicked()));
        self.configure_permission_timer(if healthy {
            HOTKEY_HEALTHY_INTERVAL_SECONDS
        } else {
            HOTKEY_RECOVERY_INTERVAL_SECONDS
        });
    }

    fn configure_permission_timer(&self, seconds: u8) {
        if self.ivars().permission_interval_seconds.get() == seconds
            && self.ivars().permission_timer.borrow().is_some()
        {
            return;
        }
        if let Some(timer) = self.ivars().permission_timer.borrow_mut().take() {
            timer.invalidate();
        }
        self.ivars().permission_interval_seconds.set(seconds);
        // SAFETY: permissionPoll: is registered above with the exact NSTimer
        // target/action signature and `self` stays retained by NSApplication.
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                f64::from(seconds),
                self,
                sel!(permissionPoll:),
                None,
                true,
            )
        };
        self.ivars().permission_timer.replace(Some(timer));
    }

    fn setup_status_item(&self) {
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSSquareStatusItemLength);
        self.ivars().status_item.replace(Some(item));
        self.rebuild_menu();
    }

    fn rebuild_menu(&self) {
        let Some(status_item) = self.ivars().status_item.borrow().clone() else {
            return;
        };
        let trusted = unsafe { AXIsProcessTrusted() };
        let screen_capture = CGPreflightScreenCaptureAccess();
        let shortcut = self.ivars().shortcut.get();
        self.ivars()
            .permission_snapshot
            .set(Some((trusted, screen_capture)));
        if let Some(button) = status_item.button(self.mtm()) {
            button.setTitle(&NSString::from_str(if trusted { "▦" } else { "!" }));
            let tooltip = if trusted {
                format!("MacFlow — {}", shortcut.label())
            } else {
                "MacFlow — Accessibility permission required".to_owned()
            };
            button.setToolTip(Some(&NSString::from_str(&tooltip)));
        }

        let menu = NSMenu::initWithTitle(NSMenu::alloc(self.mtm()), &NSString::from_str("MacFlow"));
        add_disabled(
            &menu,
            concat!("MacFlow  ", env!("CARGO_PKG_VERSION")),
            self.mtm(),
        );
        menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        let shortcut_menu =
            NSMenu::initWithTitle(NSMenu::alloc(self.mtm()), &NSString::from_str("Shortcut"));
        for choice in Shortcut::ALL {
            let selector = match choice {
                Shortcut::ControlOptionTab => sel!(shortcutControlOptionTab:),
                Shortcut::CommandTab => sel!(shortcutCommandTab:),
                Shortcut::ControlCommandTab => sel!(shortcutControlCommandTab:),
                Shortcut::ControlOptionSpace => sel!(shortcutControlOptionSpace:),
                Shortcut::CommandOptionSpace => sel!(shortcutCommandOptionSpace:),
            };
            let title = if choice == shortcut {
                format!("✓  {}", choice.label())
            } else {
                format!("    {}", choice.label())
            };
            add_action(&shortcut_menu, &title, selector, "", self, self.mtm());
        }
        let shortcut_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(self.mtm()),
                &NSString::from_str(&format!("Shortcut: {}", shortcut.label())),
                None,
                &NSString::new(),
            )
        };
        shortcut_item.setSubmenu(Some(&shortcut_menu));
        menu.addItem(&shortcut_item);

        let preferences = self.ivars().preferences.get();
        let interaction_menu = NSMenu::initWithTitle(
            NSMenu::alloc(self.mtm()),
            &NSString::from_str("Interaction"),
        );
        add_action(
            &interaction_menu,
            if preferences.quick_switch {
                "✓  Quick tap switches without chooser"
            } else {
                "    Quick tap switches without chooser"
            },
            sel!(toggleQuickSwitch:),
            "",
            self,
            self.mtm(),
        );
        interaction_menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        add_disabled(&interaction_menu, "Chooser reveal delay", self.mtm());
        for milliseconds in [70_isize, 90, 120, 150] {
            let checked = preferences.reveal_delay_ms == milliseconds as u64;
            add_tagged_action(
                &interaction_menu,
                &format!("{}  {milliseconds} ms", if checked { "✓" } else { " " }),
                sel!(setRevealDelay:),
                milliseconds,
                self,
                self.mtm(),
            );
        }
        let interaction_item = submenu_item(
            &format!("Interaction: {} ms", preferences.reveal_delay_ms),
            &interaction_menu,
            self.mtm(),
        );
        menu.addItem(&interaction_item);

        let thumbnail_menu =
            NSMenu::initWithTitle(NSMenu::alloc(self.mtm()), &NSString::from_str("Thumbnails"));
        for choice in ThumbnailPreference::ALL {
            add_tagged_action(
                &thumbnail_menu,
                &format!(
                    "{}  {}",
                    if choice == preferences.thumbnails {
                        "✓"
                    } else {
                        " "
                    },
                    choice.label()
                ),
                sel!(setThumbnailMode:),
                choice.code(),
                self,
                self.mtm(),
            );
        }
        let thumbnail_item = submenu_item(
            &format!("Thumbnails: {}", preferences.thumbnails.label()),
            &thumbnail_menu,
            self.mtm(),
        );
        menu.addItem(&thumbnail_item);

        let display_menu = NSMenu::initWithTitle(
            NSMenu::alloc(self.mtm()),
            &NSString::from_str("Chooser Display"),
        );
        for choice in DisplayTarget::ALL {
            add_tagged_action(
                &display_menu,
                &format!(
                    "{}  {}",
                    if choice == preferences.display_target {
                        "✓"
                    } else {
                        " "
                    },
                    choice.label()
                ),
                sel!(setDisplayTarget:),
                choice.code(),
                self,
                self.mtm(),
            );
        }
        let display_item = submenu_item("Chooser Display", &display_menu, self.mtm());
        menu.addItem(&display_item);

        menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        let listener_active = self
            .ivars()
            .hotkey
            .borrow()
            .as_ref()
            .is_some_and(|tap| tap.is_enabled() && !tap.handler_panicked());
        add_disabled(
            &menu,
            if self.ivars().paused.get() {
                "⏸  Shortcut listener paused"
            } else if listener_active {
                "✓  Shortcut listener active"
            } else {
                "⚠  Shortcut listener unavailable"
            },
            self.mtm(),
        );
        add_disabled(
            &menu,
            if cfg!(feature = "app-store") {
                "Cross-Space focus: public compatibility mode"
            } else {
                "Cross-Space focus: guarded exact-window mode"
            },
            self.mtm(),
        );
        add_action(
            &menu,
            if self.ivars().paused.get() {
                "Resume MacFlow"
            } else {
                "Pause MacFlow"
            },
            sel!(togglePause:),
            "",
            self,
            self.mtm(),
        );

        if !trusted {
            add_action(
                &menu,
                "⚠  Grant Accessibility access…",
                sel!(promptAccessibility:),
                "",
                self,
                self.mtm(),
            );
        }
        if screen_capture {
            add_disabled(&menu, "✓  Screen Recording enabled", self.mtm());
            add_action(
                &menu,
                "Open Screen Recording Settings…",
                sel!(openScreenRecordingSettings:),
                "",
                self,
                self.mtm(),
            );
        } else {
            add_action(
                &menu,
                "Enable Screen Recording…",
                sel!(promptScreenRecording:),
                "",
                self,
                self.mtm(),
            );
        }
        menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        let login_service = unsafe { SMAppService::mainAppService() };
        let login_status = unsafe { login_service.status() };
        let login_title = match login_status {
            SMAppServiceStatus::Enabled => "✓  Launch at Login",
            SMAppServiceStatus::RequiresApproval => "⚠  Launch at Login — approval required",
            _ => "    Launch at Login",
        };
        add_action(
            &menu,
            login_title,
            sel!(toggleLaunchAtLogin:),
            "",
            self,
            self.mtm(),
        );
        add_action(
            &menu,
            "Check for Updates…",
            sel!(checkForUpdates:),
            "",
            self,
            self.mtm(),
        );
        add_action(
            &menu,
            "Copy Diagnostics",
            sel!(copyDiagnostics:),
            "",
            self,
            self.mtm(),
        );
        menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        add_action(&menu, "Quit", sel!(quit:), "q", self, self.mtm());
        status_item.setMenu(Some(&menu));
    }
}

pub fn run() {
    let mtm = MainThreadMarker::new().expect("MacFlow must start on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
    app.setDelegate(None);
}

fn collect_apps() -> Vec<AppSeed> {
    let workspace = NSWorkspace::sharedWorkspace();
    let frontmost_pid = workspace
        .frontmostApplication()
        .map(|app| app.processIdentifier());
    let mut apps = workspace
        .runningApplications()
        .iter()
        .filter(|app| {
            app.activationPolicy() == NSApplicationActivationPolicy::Regular && !app.isTerminated()
        })
        .take(MAX_RUNNING_APPS)
        .map(|app| AppSeed {
            pid: app.processIdentifier(),
            name: app
                .localizedName()
                .map_or_else(Box::default, |name| name.to_string().into_boxed_str()),
            bundle_identifier: app
                .bundleIdentifier()
                .map(|identifier| identifier.to_string().into_boxed_str()),
        })
        .collect::<Vec<_>>();
    // Under a discovery time budget, always inspect the app the user is
    // switching away from first. The remaining order is irrelevant to the
    // later MRU/z-order sort and uses an allocation-free unstable partition.
    apps.sort_unstable_by_key(|app| app.pid != frontmost_pid.unwrap_or_default());
    apps
}

fn collect_displays() -> Vec<DisplayGeometry> {
    let mtm = MainThreadMarker::new().expect("screen query must run on main");
    let screens = NSScreen::screens(mtm);
    // `mainScreen` follows keyboard focus and can therefore be a secondary
    // monitor. Quartz's global top-left coordinates are anchored to the fixed
    // primary/menu-bar display, which AppKit exposes as screens[0]. Using the
    // focused display here shifts every display vertically as focus moves and
    // makes valid maximized windows fail the cross-display overlap test.
    let primary_frame = screens.firstObject().map_or_else(
        || NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
        |screen| screen.frame(),
    );
    let primary_max_y = primary_frame.origin.y + primary_frame.size.height;
    screens
        .iter()
        .enumerate()
        .map(|(index, screen)| {
            let frame = screen.frame();
            DisplayGeometry {
                frame: display_bounds_in_cg_space(frame, primary_max_y),
                is_main: index == 0,
            }
        })
        .collect()
}

fn display_bounds_in_cg_space(frame: NSRect, main_max_y: f64) -> Bounds {
    // NSScreen frames use a bottom-left origin while CGWindow/AX positions use
    // the global desktop's top-left origin. Convert only display geometry;
    // AX and CG window bounds already share the same coordinate system.
    Bounds::new(
        frame.origin.x,
        main_max_y - (frame.origin.y + frame.size.height),
        frame.size.width,
        frame.size.height,
    )
}

fn active_screen_frame() -> NSRect {
    let mtm = MainThreadMarker::new().expect("screen query must run on main");
    let mouse = NSEvent::mouseLocation();
    let screens = NSScreen::screens(mtm);
    screens
        .iter()
        .find(|screen| point_in_rect(mouse, screen.frame()))
        .or_else(|| NSScreen::mainScreen(mtm))
        .or_else(|| screens.firstObject())
        .map_or_else(
            || NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1280.0, 800.0)),
            |screen| screen.frame(),
        )
}

fn chooser_screen_frame(target: DisplayTarget, focused_bounds: Option<Bounds>) -> NSRect {
    let mtm = MainThreadMarker::new().expect("screen query must run on main");
    let screens = NSScreen::screens(mtm);
    let fallback = || {
        NSScreen::mainScreen(mtm)
            .or_else(|| screens.firstObject())
            .map_or_else(default_screen_frame, |screen| screen.frame())
    };
    match target {
        DisplayTarget::Mouse => active_screen_frame(),
        DisplayTarget::Main => screens
            .firstObject()
            .map_or_else(default_screen_frame, |screen| screen.frame()),
        DisplayTarget::FocusedWindow => {
            let Some(focused) = focused_bounds else {
                return fallback();
            };
            let primary_max_y = screens.firstObject().map_or(0.0, |screen| {
                let frame = screen.frame();
                frame.origin.y + frame.size.height
            });
            let mut best = None;
            let mut best_area = 0.0;
            for screen in screens.iter() {
                let frame = screen.frame();
                let display = display_bounds_in_cg_space(frame, primary_max_y);
                let area = bounds_overlap_area(focused, display);
                if area > best_area {
                    best = Some(frame);
                    best_area = area;
                }
            }
            best.unwrap_or_else(fallback)
        }
    }
}

fn default_screen_frame() -> NSRect {
    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1280.0, 800.0))
}

fn bounds_overlap_area(left: Bounds, right: Bounds) -> f64 {
    let width = (left.x + left.width).min(right.x + right.width) - left.x.max(right.x);
    let height = (left.y + left.height).min(right.y + right.height) - left.y.max(right.y);
    width.max(0.0) * height.max(0.0)
}

fn point_in_rect(point: NSPoint, rect: NSRect) -> bool {
    point.x >= rect.origin.x
        && point.x < rect.origin.x + rect.size.width
        && point.y >= rect.origin.y
        && point.y < rect.origin.y + rect.size.height
}

fn add_disabled(menu: &NSMenu, title: &str, mtm: MainThreadMarker) {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            None,
            &NSString::new(),
        )
    };
    item.setEnabled(false);
    menu.addItem(&item);
}

fn add_action(
    menu: &NSMenu,
    title: &str,
    action: objc2::runtime::Sel,
    key: &str,
    target: &AppDelegate,
    mtm: MainThreadMarker,
) {
    // SAFETY: Each selector is implemented by AppDelegate above with the
    // standard one-argument action signature.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    };
    unsafe { item.setTarget(Some(target)) };
    menu.addItem(&item);
}

fn add_tagged_action(
    menu: &NSMenu,
    title: &str,
    action: objc2::runtime::Sel,
    tag: isize,
    target: &AppDelegate,
    mtm: MainThreadMarker,
) {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::new(),
        )
    };
    item.setTag(tag);
    unsafe { item.setTarget(Some(target)) };
    menu.addItem(&item);
}

fn submenu_item(title: &str, submenu: &NSMenu, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            None,
            &NSString::new(),
        )
    };
    item.setSubmenu(Some(submenu));
    item
}

fn sender_tag(sender: Option<&AnyObject>) -> Option<isize> {
    sender?.downcast_ref::<NSMenuItem>().map(NSMenuItem::tag)
}

fn open_settings(url: &str) {
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(url)) {
        let _ = NSWorkspace::sharedWorkspace().openURL(&url);
    }
}

fn copy_to_clipboard(value: &str) {
    let pasteboard = NSPasteboard::generalPasteboard();
    let _ = pasteboard.clearContents();
    let _ = pasteboard.setString_forType(&NSString::from_str(value), unsafe {
        NSPasteboardTypeString
    });
}

/// LaunchServices can run two copies of the same menu-bar app when one is in
/// `/Applications` and another is opened from a freshly rebuilt bundle. Two
/// global event taps then race to interpret the same chord, and an installed
/// older copy can silently retain an obsolete shortcut. The newly launched
/// trusted copy is authoritative and asks every older MacFlow instance to
/// quit before it installs its own tap. The caller must first establish that
/// this process has Accessibility access, so an untrusted development build
/// cannot take down a working installed copy.
fn terminate_other_instances() {
    let current_pid = NSRunningApplication::currentApplication().processIdentifier();
    let applications = NSRunningApplication::runningApplicationsWithBundleIdentifier(
        &NSString::from_str(BUNDLE_IDENTIFIER),
    );
    for application in applications.iter() {
        if application.processIdentifier() != current_pid {
            let _ = application.terminate();
        }
    }
}

fn load_shortcut() -> Shortcut {
    let defaults = NSUserDefaults::standardUserDefaults();
    Shortcut::from_code(defaults.integerForKey(&NSString::from_str(SHORTCUT_DEFAULTS_KEY)))
}

fn save_shortcut(shortcut: Shortcut) {
    NSUserDefaults::standardUserDefaults()
        .setInteger_forKey(shortcut.code(), &NSString::from_str(SHORTCUT_DEFAULTS_KEY));
}

fn load_preferences() -> Preferences {
    let defaults = NSUserDefaults::standardUserDefaults();
    let mut preferences = Preferences::default();

    let reveal_key = NSString::from_str(REVEAL_DELAY_DEFAULTS_KEY);
    if defaults.objectForKey(&reveal_key).is_some() {
        preferences.reveal_delay_ms = u64::try_from(defaults.integerForKey(&reveal_key))
            .unwrap_or(DEFAULT_REVEAL_DELAY_MS)
            .clamp(0, 500);
    }
    let quick_key = NSString::from_str(QUICK_SWITCH_DEFAULTS_KEY);
    if defaults.objectForKey(&quick_key).is_some() {
        preferences.quick_switch = defaults.boolForKey(&quick_key);
    }
    let thumbnails_key = NSString::from_str(THUMBNAIL_MODE_DEFAULTS_KEY);
    if defaults.objectForKey(&thumbnails_key).is_some() {
        preferences.thumbnails =
            ThumbnailPreference::from_code(defaults.integerForKey(&thumbnails_key));
    }
    let display_key = NSString::from_str(DISPLAY_TARGET_DEFAULTS_KEY);
    if defaults.objectForKey(&display_key).is_some() {
        preferences.display_target = DisplayTarget::from_code(defaults.integerForKey(&display_key));
    }
    preferences
}

fn save_preferences(preferences: Preferences) {
    let defaults = NSUserDefaults::standardUserDefaults();
    defaults.setInteger_forKey(
        isize::try_from(preferences.reveal_delay_ms).unwrap_or(500),
        &NSString::from_str(REVEAL_DELAY_DEFAULTS_KEY),
    );
    defaults.setBool_forKey(
        preferences.quick_switch,
        &NSString::from_str(QUICK_SWITCH_DEFAULTS_KEY),
    );
    defaults.setInteger_forKey(
        preferences.thumbnails.code(),
        &NSString::from_str(THUMBNAIL_MODE_DEFAULTS_KEY),
    );
    defaults.setInteger_forKey(
        preferences.display_target.code(),
        &NSString::from_str(DISPLAY_TARGET_DEFAULTS_KEY),
    );
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait<'a, T>(condition: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condition
        .wait(guard)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn presentation_never_uses_an_old_or_empty_discovery_snapshot() {
        assert!(!discovery_is_fresh(false, Some(Duration::ZERO)));
        assert!(!discovery_is_fresh(true, None));
        assert!(discovery_is_fresh(true, Some(FRESH_DISCOVERY_MAX_AGE)));
        assert!(!discovery_is_fresh(
            true,
            Some(FRESH_DISCOVERY_MAX_AGE + Duration::from_millis(1))
        ));
    }

    #[test]
    fn changed_live_window_identity_invalidates_an_otherwise_fresh_cache() {
        let cached_ids = [Some(10), Some(20), None];
        assert!(origin_is_represented(
            &cached_ids,
            |window_id| *window_id,
            Some(20)
        ));
        assert!(!origin_is_represented(
            &cached_ids,
            |window_id| *window_id,
            Some(30)
        ));
        assert!(origin_is_represented(
            &cached_ids,
            |window_id| *window_id,
            None
        ));
    }

    #[test]
    fn pending_navigation_wraps_without_overflow_or_losing_distinct_presses() {
        assert_eq!(selection_after_pending_navigation(1, 4, 0, 0), 1);
        assert_eq!(selection_after_pending_navigation(1, 4, 1, 0), 2);
        assert_eq!(selection_after_pending_navigation(1, 4, 3, 0), 0);
        assert_eq!(selection_after_pending_navigation(1, 4, 0, 1), 0);
        assert_eq!(selection_after_pending_navigation(1, 4, u64::MAX, 0), 0);
        assert_eq!(selection_after_pending_navigation(7, 0, u64::MAX, 9), 0);

        let mut forward = 0_u64;
        let mut backward = 0_u64;
        for index in 0..250_000_u64 {
            if index % 3 == 0 {
                backward = backward.wrapping_add(1);
            } else {
                forward = forward.wrapping_add(1);
            }
            assert!(selection_after_pending_navigation(1, 192, forward, backward) < 192);
        }
    }

    #[test]
    fn delayed_overlay_rejects_release_and_stale_gesture_callbacks() {
        assert_eq!(
            Preferences::default().reveal_delay(),
            Duration::from_millis(90)
        );
        assert!(overlay_delay_is_current(8, 8, Phase::AwaitingWindows));
        assert!(overlay_delay_is_current(8, 8, Phase::Visible));
        assert!(!overlay_delay_is_current(7, 8, Phase::Visible));
        assert!(!overlay_delay_is_current(8, 8, Phase::Hidden));
        assert!(!overlay_delay_is_current(8, 8, Phase::Hiding));
    }

    #[test]
    fn reveal_requires_the_configured_modifiers_to_still_be_physically_held() {
        let cases = [
            (Shortcut::CommandTab, NSEventModifierFlags::Command),
            (
                Shortcut::ControlOptionTab,
                NSEventModifierFlags::Control | NSEventModifierFlags::Option,
            ),
            (
                Shortcut::ControlCommandTab,
                NSEventModifierFlags::Control | NSEventModifierFlags::Command,
            ),
            (
                Shortcut::ControlOptionSpace,
                NSEventModifierFlags::Control | NSEventModifierFlags::Option,
            ),
            (
                Shortcut::CommandOptionSpace,
                NSEventModifierFlags::Command | NSEventModifierFlags::Option,
            ),
        ];
        for (shortcut, flags) in cases {
            assert!(shortcut_matches_flags(shortcut, flags));
            assert!(shortcut_matches_flags(
                shortcut,
                flags | NSEventModifierFlags::Shift,
            ));
            assert!(!reveal_input_matches_flags(false, shortcut, flags));
        }
        assert!(!shortcut_matches_flags(
            Shortcut::CommandTab,
            NSEventModifierFlags::empty(),
        ));
        assert!(!shortcut_matches_flags(
            Shortcut::CommandTab,
            NSEventModifierFlags::Command | NSEventModifierFlags::Control,
        ));
        assert!(!shortcut_matches_flags(
            Shortcut::ControlOptionTab,
            NSEventModifierFlags::Control,
        ));
    }

    #[test]
    fn synchronous_gesture_latch_tracks_the_newest_input_not_drain_order() {
        assert_eq!(gesture_activity_update(Action::Show), Some(true));
        assert_eq!(gesture_activity_update(Action::ShowPrevious), Some(true));
        assert_eq!(gesture_activity_update(Action::Confirm), Some(false));
        assert_eq!(gesture_activity_update(Action::Cancel), Some(false));
        assert_eq!(gesture_activity_update(Action::ReenableTap), Some(false));
        assert_eq!(gesture_activity_update(Action::Next), None);

        // An old terminal action may still be waiting in the mailbox when a
        // second physical gesture starts. The newest synchronous input remains
        // authoritative while those older actions drain.
        let mut active = false;
        for action in [Action::Show, Action::Confirm, Action::Show] {
            if let Some(updated) = gesture_activity_update(action) {
                active = updated;
            }
        }
        assert!(active);
    }

    fn drain_model_batch(mailbox: &mut ActionMailbox, model: &mut VecDeque<Action>) {
        assert!(mailbox.begin_drain());
        for _ in 0..ACTION_DRAIN_BATCH {
            let actual = mailbox.pop_front();
            let expected = model.pop_front();
            assert_eq!(actual, expected);
            if actual.is_none() {
                break;
            }
        }
        assert_eq!(mailbox.finish_drain(), !model.is_empty());
    }

    #[test]
    fn action_mailbox_is_fifo_and_schedules_only_once_while_pending() {
        let mut mailbox = ActionMailbox::default();
        let actions = [
            Action::PreWarm,
            Action::Show,
            Action::Next,
            Action::Previous,
            Action::Confirm,
        ];

        for (index, action) in actions.into_iter().enumerate() {
            assert_eq!(
                mailbox.enqueue(action),
                if index == 0 {
                    EnqueueResult::QueuedAndSchedule
                } else {
                    EnqueueResult::Queued
                }
            );
        }

        assert!(mailbox.begin_drain());
        for expected in actions {
            assert_eq!(mailbox.pop_front(), Some(expected));
        }
        assert_eq!(mailbox.pop_front(), None);
        assert!(!mailbox.finish_drain());
    }

    #[test]
    fn action_arriving_during_drain_gets_one_followup_batch() {
        let mut mailbox = ActionMailbox::default();
        assert_eq!(
            mailbox.enqueue(Action::Show),
            EnqueueResult::QueuedAndSchedule
        );
        assert!(mailbox.begin_drain());
        assert_eq!(mailbox.pop_front(), Some(Action::Show));

        // Models a nested event-tap callback while AppKit is processing the
        // first action. It must not schedule a second concurrent drain.
        assert_eq!(mailbox.enqueue(Action::Next), EnqueueResult::Queued);
        assert!(mailbox.finish_drain());
        assert!(mailbox.begin_drain());
        assert_eq!(mailbox.pop_front(), Some(Action::Next));
        assert!(!mailbox.finish_drain());
    }

    #[test]
    fn terminal_action_survives_a_full_navigation_mailbox() {
        let mut mailbox = ActionMailbox::default();
        assert_eq!(
            mailbox.enqueue(Action::Show),
            EnqueueResult::QueuedAndSchedule
        );
        for index in 1..ACTION_MAILBOX_CAPACITY {
            let action = if index % 2 == 0 {
                Action::Next
            } else {
                Action::Previous
            };
            assert_eq!(mailbox.enqueue(action), EnqueueResult::Queued);
        }

        assert_eq!(mailbox.len, ACTION_MAILBOX_CAPACITY);
        assert_eq!(mailbox.enqueue(Action::Next), EnqueueResult::Dropped);
        assert_eq!(mailbox.enqueue(Action::Confirm), EnqueueResult::Queued);
        assert_eq!(mailbox.len, ACTION_MAILBOX_CAPACITY);

        assert!(mailbox.begin_drain());
        assert_eq!(mailbox.pop_front(), Some(Action::Show));
        let mut last = None;
        while let Some(action) = mailbox.pop_front() {
            last = Some(action);
        }
        assert_eq!(last, Some(Action::Confirm));
        assert!(!mailbox.finish_drain());
    }

    #[test]
    fn action_mailbox_matches_fifo_model_for_250k_operations() {
        let mut mailbox = ActionMailbox::default();
        let mut model = VecDeque::with_capacity(ACTION_MAILBOX_CAPACITY);
        let actions = [
            Action::PreWarm,
            Action::Show,
            Action::Next,
            Action::Previous,
            Action::Confirm,
            Action::Cancel,
            Action::ReenableTap,
        ];

        for index in 0..250_000 {
            let action = actions[index % actions.len()];
            let result = mailbox.enqueue(action);
            assert!(matches!(
                result,
                EnqueueResult::Queued | EnqueueResult::QueuedAndSchedule
            ));
            model.push_back(action);

            // This cadence keeps the producer ahead of the consumer without
            // reaching overflow, exercising wrap-around and scheduling state.
            if index % 11 == 0 {
                drain_model_batch(&mut mailbox, &mut model);
            }
        }

        while !model.is_empty() {
            drain_model_batch(&mut mailbox, &mut model);
        }
        assert_eq!(mailbox.len, 0);
        assert!(!mailbox.drain_scheduled);
        assert!(!mailbox.draining);
    }

    #[test]
    fn closed_action_mailbox_rejects_and_discards_work() {
        let mut mailbox = ActionMailbox::default();
        assert_eq!(
            mailbox.enqueue(Action::Show),
            EnqueueResult::QueuedAndSchedule
        );
        mailbox.close();
        assert_eq!(mailbox.enqueue(Action::Cancel), EnqueueResult::Closed);
        assert_eq!(mailbox.pop_front(), None);
        assert!(!mailbox.begin_drain());
    }

    #[test]
    fn screen_hit_test_handles_negative_monitor_origins() {
        let rect = NSRect::new(NSPoint::new(-1440.0, 80.0), NSSize::new(1440.0, 900.0));
        assert!(point_in_rect(NSPoint::new(-100.0, 100.0), rect));
        assert!(!point_in_rect(NSPoint::new(1.0, 100.0), rect));
    }

    #[test]
    fn discovery_pending_slot_keeps_only_latest_request() {
        let state = Mutex::new(DiscoveryQueueState::default());
        let first = DiscoveryRequest {
            sequence: 1,
            apps: Vec::new(),
            displays: vec![DisplayGeometry {
                frame: Bounds::new(0.0, 0.0, 1.0, 1.0),
                is_main: true,
            }],
        };
        let second = DiscoveryRequest {
            sequence: 2,
            apps: Vec::new(),
            displays: vec![DisplayGeometry {
                frame: Bounds::new(0.0, 0.0, 2.0, 2.0),
                is_main: true,
            }],
        };
        lock(&state).pending = Some(first);
        lock(&state).pending = Some(second);
        assert_eq!(
            lock(&state)
                .pending
                .as_ref()
                .map(|request| request.displays[0].frame.width),
            Some(2.0)
        );
        assert_eq!(
            lock(&state)
                .pending
                .as_ref()
                .map(|request| request.sequence),
            Some(2)
        );
    }

    #[test]
    fn discovery_cancellation_observes_newer_work_and_shutdown() {
        let shared = DiscoveryShared {
            state: Mutex::new(DiscoveryQueueState::default()),
            wake: Condvar::new(),
            delivery: Delivery::new(),
        };
        assert!(!shared.should_cancel());
        lock(&shared.state).pending = Some(DiscoveryRequest {
            sequence: 3,
            apps: Vec::new(),
            displays: Vec::new(),
        });
        assert!(shared.should_cancel());
        {
            let mut state = lock(&shared.state);
            state.pending = None;
            state.stopping = true;
        }
        assert!(shared.should_cancel());
    }

    #[test]
    fn display_bounds_convert_stacked_screens_to_cg_coordinates() {
        let primary_max_y = 1080.0;
        let main = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1920.0, 1080.0));
        let above = NSRect::new(NSPoint::new(0.0, 1080.0), NSSize::new(1920.0, 900.0));
        let below = NSRect::new(NSPoint::new(0.0, -900.0), NSSize::new(1920.0, 900.0));

        assert_eq!(
            display_bounds_in_cg_space(main, primary_max_y),
            Bounds::new(0.0, 0.0, 1920.0, 1080.0)
        );
        assert_eq!(
            display_bounds_in_cg_space(above, primary_max_y),
            Bounds::new(0.0, -900.0, 1920.0, 900.0)
        );
        assert_eq!(
            display_bounds_in_cg_space(below, primary_max_y),
            Bounds::new(0.0, 1080.0, 1920.0, 900.0)
        );

        // Keyboard focus can make the upper secondary display `mainScreen`.
        // Its maxY is not the Quartz reference: using it would move the fixed
        // primary display down by 900 points and break overlap matching.
        let focused_secondary_max_y = above.origin.y + above.size.height;
        assert_eq!(
            display_bounds_in_cg_space(main, focused_secondary_max_y),
            Bounds::new(0.0, 900.0, 1920.0, 1080.0)
        );
    }
}
