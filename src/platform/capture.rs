//! Memory-bounded, serial thumbnail capture.
//!
//! The scheduler intentionally has one pending batch and one capture in flight.
//! A newer batch preempts the remainder of the current batch, which keeps the
//! selected/visible windows responsive without allowing work to pile up.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use objc2::rc::autoreleasepool;
use objc2_core_foundation::CFRetained;
#[cfg(feature = "modern-capture")]
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGImage;
#[cfg(feature = "modern-capture")]
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImageAlphaInfo,
    CGInterpolationQuality,
};

use crate::core::cache::{InsertResult, TinyLru};
use crate::core::model::WindowKey;

pub const DEFAULT_CACHE_BUDGET_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TARGETS_PER_BATCH: usize = 12;
pub const MAX_THUMBNAIL_WIDTH: u16 = 560;
pub const MAX_THUMBNAIL_HEIGHT: u16 = 336;

const FAILURE_BACKOFF: Duration = Duration::from_millis(900);
const MAX_FAILURES: usize = 32;
const WORKER_STACK_BYTES: usize = 512 * 1024;

/// Controls how aggressively the bounded capture worker refreshes thumbnails.
///
/// The first submitted target is always the selected window. Remaining targets
/// are visible/near-visible cards in priority order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum CaptureMode {
    /// Keep existing cached images available, but perform no new capture work.
    Off = 0,
    /// Capture or refresh only the selected (first) target.
    Selected = 1,
    /// Keep the selected target fresh and fill missing visible thumbnails.
    Visible = 2,
    /// Keep selected and visible thumbnails within their respective max ages.
    #[default]
    Fresh = 3,
}

impl CaptureMode {
    const fn from_raw(value: u8) -> Self {
        match value {
            0 => Self::Off,
            1 => Self::Selected,
            2 => Self::Visible,
            _ => Self::Fresh,
        }
    }
}

/// The role of a thumbnail in the current chooser viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureRole {
    Selected,
    Visible,
}

/// Freshness returned for a cached thumbnail. A stale image remains usable for
/// immediate presentation while the serial worker refreshes it in background.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbnailStatus {
    Missing,
    Fresh,
    Stale,
}

/// Independent expiry windows keep the selected preview responsive without
/// repeatedly recapturing every visible card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreshnessPolicy {
    pub selected_max_age: Duration,
    pub visible_max_age: Duration,
}

impl Default for FreshnessPolicy {
    fn default() -> Self {
        Self {
            selected_max_age: Duration::from_millis(650),
            visible_max_age: Duration::from_secs(3),
        }
    }
}

impl FreshnessPolicy {
    const MIN_SELECTED_MAX_AGE: Duration = Duration::from_millis(100);
    const MIN_VISIBLE_MAX_AGE: Duration = Duration::from_millis(250);

    fn normalized(self) -> Self {
        let selected_max_age = self.selected_max_age.max(Self::MIN_SELECTED_MAX_AGE);
        let visible_max_age = self
            .visible_max_age
            .max(Self::MIN_VISIBLE_MAX_AGE)
            .max(selected_max_age);
        Self {
            selected_max_age,
            visible_max_age,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureSize {
    pub width: u16,
    pub height: u16,
}

impl CaptureSize {
    #[must_use]
    pub fn bounded(width: u16, height: u16) -> Self {
        Self {
            width: width.clamp(1, MAX_THUMBNAIL_WIDTH),
            height: height.clamp(1, MAX_THUMBNAIL_HEIGHT),
        }
    }

    /// Returns the largest capture canvas that fits the thumbnail limits while
    /// retaining the source window's aspect ratio. ScreenCaptureKit otherwise
    /// returns a fixed 560×336 canvas for every window; narrow or compact
    /// windows can then be positioned within that canvas and appear offset
    /// when the UI aspect-fits the canvas rather than the actual window.
    #[must_use]
    pub fn preserving_aspect_ratio(window_width: f64, window_height: f64) -> Self {
        if !window_width.is_finite()
            || !window_height.is_finite()
            || window_width <= 0.0
            || window_height <= 0.0
        {
            return Self::default();
        }

        // Normalize before scaling so extremely large, but still finite,
        // accessibility bounds cannot overflow the ratio calculation.
        let largest_dimension = window_width.max(window_height);
        let normalized_width = window_width / largest_dimension;
        let normalized_height = window_height / largest_dimension;
        let scale = (f64::from(MAX_THUMBNAIL_WIDTH) / normalized_width)
            .min(f64::from(MAX_THUMBNAIL_HEIGHT) / normalized_height);
        let width = (normalized_width * scale)
            .round()
            .clamp(1.0, f64::from(MAX_THUMBNAIL_WIDTH)) as u16;
        let height = (normalized_height * scale)
            .round()
            .clamp(1.0, f64::from(MAX_THUMBNAIL_HEIGHT)) as u16;
        Self::bounded(width, height)
    }
}

impl Default for CaptureSize {
    fn default() -> Self {
        Self::bounded(MAX_THUMBNAIL_WIDTH, MAX_THUMBNAIL_HEIGHT)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureTarget {
    pub key: WindowKey,
    pub size: CaptureSize,
}

impl CaptureTarget {
    #[must_use]
    pub fn new(key: WindowKey, size: CaptureSize) -> Self {
        Self {
            key,
            size: CaptureSize::bounded(size.width, size.height),
        }
    }
}

#[derive(Clone)]
struct Thumbnail {
    image: CFRetained<CGImage>,
    cost_bytes: usize,
    requested_size: CaptureSize,
    captured_at: Instant,
}

impl Thumbnail {
    fn new(image: CFRetained<CGImage>, requested_size: CaptureSize) -> Self {
        let cost_bytes = image_cost(&image);
        Self {
            image,
            cost_bytes,
            requested_size,
            captured_at: Instant::now(),
        }
    }

    #[must_use]
    pub const fn cost_bytes(&self) -> usize {
        self.cost_bytes
    }
}

impl std::fmt::Debug for Thumbnail {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Thumbnail")
            .field("width", &CGImage::width(Some(&self.image)))
            .field("height", &CGImage::height(Some(&self.image)))
            .field("cost_bytes", &self.cost_bytes)
            .field("requested_size", &self.requested_size)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubmitResult {
    pub accepted: usize,
    pub coalesced: usize,
    pub replaced_pending_batch: bool,
    pub stale_generation: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureStats {
    pub cache_entries: usize,
    pub cache_bytes: usize,
    pub cache_budget_bytes: usize,
    pub pending_targets: usize,
    pub capture_in_flight: bool,
    pub ready_sequence: u64,
    pub update_pending: bool,
    pub mode: CaptureMode,
}

#[derive(Clone, Debug)]
struct CaptureBatch {
    generation: u64,
    revision: u64,
    selected_key: Option<WindowKey>,
    targets: Vec<CaptureTarget>,
}

#[cfg(any(feature = "modern-capture", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RequestedWindowKey {
    pid: i32,
    window_id: u32,
}

#[cfg(any(feature = "modern-capture", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RequestedWindows {
    keys: [RequestedWindowKey; MAX_TARGETS_PER_BATCH],
    len: usize,
}

#[cfg(any(feature = "modern-capture", test))]
impl RequestedWindows {
    fn from_targets(targets: &[CaptureTarget]) -> Self {
        let mut keys = [RequestedWindowKey {
            pid: 0,
            window_id: 0,
        }; MAX_TARGETS_PER_BATCH];
        let mut len = 0;
        for target in targets {
            let Some(window_id) = target.key.window_id else {
                continue;
            };
            let key = RequestedWindowKey {
                pid: target.key.pid,
                window_id,
            };
            if len == keys.len() {
                break;
            }
            if keys[..len].contains(&key) {
                continue;
            }
            keys[len] = key;
            len += 1;
        }
        Self { keys, len }
    }

    fn contains(self, key: RequestedWindowKey) -> bool {
        self.keys[..self.len].contains(&key)
    }

    const fn len(self) -> usize {
        self.len
    }

    #[cfg(feature = "modern-capture")]
    const fn is_empty(self) -> bool {
        self.len == 0
    }
}

struct BatchCaptureContext {
    #[cfg(feature = "modern-capture")]
    snapshot: Option<modern::Snapshot>,
}

impl BatchCaptureContext {
    fn acquire(targets: &[CaptureTarget]) -> Self {
        #[cfg(feature = "modern-capture")]
        {
            let requested = RequestedWindows::from_targets(targets);
            Self {
                snapshot: modern::Snapshot::acquire(requested),
            }
        }
        #[cfg(not(feature = "modern-capture"))]
        {
            let _ = targets;
            Self {}
        }
    }

    fn capture(&self, target: &CaptureTarget) -> Option<CFRetained<CGImage>> {
        #[cfg(feature = "modern-capture")]
        {
            self.snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.capture(target))
                .and_then(|image| bound_image(image, target.size))
        }
        #[cfg(not(feature = "modern-capture"))]
        {
            let _ = target;
            // Thumbnail capture is optional. The UI draws the app icon when
            // the modern backend was intentionally not compiled in.
            None
        }
    }
}

#[derive(Debug, Default)]
struct QueueState {
    pending: Option<CaptureBatch>,
    in_flight: Option<(u64, u64, CaptureTarget)>,
    worker_busy: bool,
    stopping: bool,
}

#[derive(Debug, Default)]
struct WorkQueue {
    state: Mutex<QueueState>,
    wake: Condvar,
}

enum QueueWake {
    Batch(CaptureBatch),
    RetryDue,
    Stopped,
}

impl WorkQueue {
    fn submit(
        &self,
        batch: CaptureBatch,
        current_generation: &AtomicU64,
        current_revision: &AtomicU64,
    ) -> SubmitResult {
        let mut state = lock(&self.state);
        // Read while holding the queue lock. This orders submission against
        // begin_generation's post-increment queue cleanup and prevents an old
        // producer from replacing a newer valid batch.
        if state.stopping
            || batch.generation != current_generation.load(Ordering::Acquire)
            || batch.revision != current_revision.load(Ordering::Acquire)
        {
            return SubmitResult {
                stale_generation: true,
                ..SubmitResult::default()
            };
        }

        let mut selected_key = batch.selected_key;
        let incoming = batch.targets;
        let mut merged = Vec::with_capacity(MAX_TARGETS_PER_BATCH);
        let mut coalesced = 0;
        for target in &incoming {
            let is_in_flight =
                state
                    .in_flight
                    .as_ref()
                    .is_some_and(|(generation, revision, active)| {
                        *generation == batch.generation
                            && *revision == batch.revision
                            && active == target
                    });
            if is_in_flight
                || merged
                    .iter()
                    .any(|item: &CaptureTarget| item.key == target.key)
            {
                coalesced += 1;
            } else if merged.len() < MAX_TARGETS_PER_BATCH {
                merged.push(target.clone());
            }
        }

        let previous = state.pending.take();
        let replaced_pending_batch = previous.is_some();
        if let Some(previous) = previous
            && previous.generation == batch.generation
            && previous.revision == batch.revision
        {
            if selected_key.is_none() {
                selected_key = previous.selected_key;
            }
            for target in previous.targets {
                if merged.len() == MAX_TARGETS_PER_BATCH {
                    break;
                }
                // Every key in the new viewport is authoritative, including
                // an exact request already in flight. Do not resurrect an
                // older pending size for that key after coalescing the new
                // request against the active capture.
                if incoming.iter().any(|item| item.key == target.key)
                    || merged.iter().any(|item| item.key == target.key)
                {
                    coalesced += 1;
                } else {
                    merged.push(target);
                }
            }
        }

        let accepted = merged.len();
        if accepted != 0 {
            state.pending = Some(CaptureBatch {
                generation: batch.generation,
                revision: batch.revision,
                selected_key,
                targets: merged,
            });
            self.wake.notify_one();
        }

        SubmitResult {
            accepted,
            coalesced,
            replaced_pending_batch,
            stale_generation: false,
        }
    }

    fn take_until(&self, retry_after: Option<Instant>) -> QueueWake {
        let mut state = lock(&self.state);
        loop {
            if state.stopping {
                return QueueWake::Stopped;
            }
            if let Some(batch) = state.pending.take() {
                state.worker_busy = true;
                return QueueWake::Batch(batch);
            }
            if let Some(deadline) = retry_after {
                let now = Instant::now();
                if now >= deadline {
                    return QueueWake::RetryDue;
                }
                state = wait_timeout(&self.wake, state, deadline.saturating_duration_since(now));
            } else {
                state = wait(&self.wake, state);
            }
        }
    }

    #[cfg(test)]
    fn take(&self) -> Option<CaptureBatch> {
        match self.take_until(None) {
            QueueWake::Batch(batch) => Some(batch),
            QueueWake::RetryDue | QueueWake::Stopped => None,
        }
    }

    fn finish_batch(&self) {
        lock(&self.state).worker_busy = false;
    }

    fn set_in_flight(&self, generation: u64, revision: u64, target: CaptureTarget) {
        lock(&self.state).in_flight = Some((generation, revision, target));
    }

    fn clear_in_flight(&self, generation: u64, revision: u64, target: &CaptureTarget) {
        let mut state = lock(&self.state);
        if state.in_flight.as_ref().is_some_and(
            |(active_generation, active_revision, active_target)| {
                *active_generation == generation
                    && *active_revision == revision
                    && active_target == target
            },
        ) {
            state.in_flight = None;
        }
    }

    fn should_preempt(&self) -> bool {
        let state = lock(&self.state);
        state.stopping || state.pending.is_some()
    }

    fn cancel_pending(&self) {
        lock(&self.state).pending = None;
    }

    fn discard_pending_other_than(&self, generation: u64) {
        let mut state = lock(&self.state);
        if state
            .pending
            .as_ref()
            .is_some_and(|batch| batch.generation != generation)
        {
            state.pending = None;
        }
    }

    fn discard_pending_request(&self, generation: u64, revision: u64) {
        let mut state = lock(&self.state);
        if state
            .pending
            .as_ref()
            .is_some_and(|batch| batch.generation == generation && batch.revision == revision)
        {
            state.pending = None;
        }
    }

    fn stop(&self) {
        let mut state = lock(&self.state);
        state.stopping = true;
        state.pending = None;
        self.wake.notify_one();
    }
}

#[derive(Debug)]
struct FailedCapture {
    generation: u64,
    revision: u64,
    target: CaptureTarget,
    retry_after: Instant,
}

type UpdateHandler = Arc<dyn Fn() + Send + Sync + 'static>;

#[derive(Default)]
struct UpdateSignalState {
    handler: Option<UpdateHandler>,
    callback_issued: bool,
}

/// Coalesces arbitrarily many worker publications into one outstanding
/// callback. The consumer acknowledges before reading stats, closing the race
/// between a final image publication and the worker becoming idle.
#[derive(Default)]
struct UpdateSignal {
    pending: AtomicBool,
    state: Mutex<UpdateSignalState>,
}

impl UpdateSignal {
    fn set_handler(&self, handler: UpdateHandler) {
        let callback = {
            let mut state = lock(&self.state);
            state.handler = Some(handler);
            if self.pending.load(Ordering::Acquire) && !state.callback_issued {
                state.callback_issued = true;
                state.handler.clone()
            } else {
                None
            }
        };
        if let Some(callback) = callback {
            callback();
        }
    }

    fn clear_handler(&self) {
        let mut state = lock(&self.state);
        state.handler = None;
        state.callback_issued = false;
    }

    fn publish(&self) {
        let callback = {
            let mut state = lock(&self.state);
            if self.pending.load(Ordering::Acquire) {
                return;
            }
            self.pending.store(true, Ordering::Release);
            let callback = state.handler.clone();
            state.callback_issued = callback.is_some();
            callback
        };
        if let Some(callback) = callback {
            callback();
        }
    }

    fn acknowledge(&self) {
        let mut state = lock(&self.state);
        self.pending.store(false, Ordering::Release);
        state.callback_issued = false;
    }

    fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }
}

struct Shared {
    generation: AtomicU64,
    revision: AtomicU64,
    ready_sequence: AtomicU64,
    mode: AtomicU8,
    freshness: FreshnessPolicy,
    cache: Mutex<TinyLru<WindowKey, Thumbnail>>,
    failures: Mutex<Vec<FailedCapture>>,
    queue: WorkQueue,
    updates: UpdateSignal,
}

fn role_for_index(index: usize) -> CaptureRole {
    if index == 0 {
        CaptureRole::Selected
    } else {
        CaptureRole::Visible
    }
}

fn max_age_for_role(policy: FreshnessPolicy, role: CaptureRole) -> Duration {
    match role {
        CaptureRole::Selected => policy.selected_max_age,
        CaptureRole::Visible => policy.visible_max_age,
    }
}

fn thumbnail_status_at(
    thumbnail: Option<&Thumbnail>,
    requested_size: CaptureSize,
    role: CaptureRole,
    policy: FreshnessPolicy,
    now: Instant,
) -> ThumbnailStatus {
    thumbnail_status_from_metadata(
        thumbnail.map(|thumbnail| (thumbnail.requested_size, thumbnail.captured_at)),
        requested_size,
        role,
        policy,
        now,
    )
}

fn thumbnail_status_from_metadata(
    thumbnail: Option<(CaptureSize, Instant)>,
    requested_size: CaptureSize,
    role: CaptureRole,
    policy: FreshnessPolicy,
    now: Instant,
) -> ThumbnailStatus {
    let Some((cached_size, captured_at)) = thumbnail else {
        return ThumbnailStatus::Missing;
    };
    if cached_size != requested_size {
        return ThumbnailStatus::Missing;
    }
    if now.saturating_duration_since(captured_at) <= max_age_for_role(policy, role) {
        ThumbnailStatus::Fresh
    } else {
        ThumbnailStatus::Stale
    }
}

fn mode_requests_capture(mode: CaptureMode, index: usize, status: ThumbnailStatus) -> bool {
    match mode {
        CaptureMode::Off => false,
        CaptureMode::Selected => index == 0 && status != ThumbnailStatus::Fresh,
        CaptureMode::Visible => {
            if index == 0 {
                status != ThumbnailStatus::Fresh
            } else {
                status == ThumbnailStatus::Missing
            }
        }
        CaptureMode::Fresh => status != ThumbnailStatus::Fresh,
    }
}

fn retain_live_cache_entries<V>(cache: &mut TinyLru<WindowKey, V>, live_keys: &[WindowKey]) {
    cache.retain(|key| live_keys.contains(key));
}

fn invalidate_cache_entries<V>(cache: &mut TinyLru<WindowKey, V>, keys: &[WindowKey]) -> usize {
    let before = cache.len();
    cache.retain(|key| !keys.contains(key));
    before - cache.len()
}

/// Owns exactly one capture worker. `get` is safe from the main thread and
/// only returns a retained Core Graphics image; it never touches AppKit.
pub struct CapturePipeline {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl CapturePipeline {
    pub fn new() -> io::Result<Self> {
        Self::with_budget(DEFAULT_CACHE_BUDGET_BYTES)
    }

    pub fn with_budget(budget_bytes: usize) -> io::Result<Self> {
        Self::with_budget_and_freshness(budget_bytes, FreshnessPolicy::default())
    }

    pub fn with_budget_and_freshness(
        budget_bytes: usize,
        freshness: FreshnessPolicy,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            generation: AtomicU64::new(0),
            revision: AtomicU64::new(0),
            ready_sequence: AtomicU64::new(0),
            mode: AtomicU8::new(CaptureMode::default() as u8),
            freshness: freshness.normalized(),
            cache: Mutex::new(TinyLru::new(budget_bytes)),
            failures: Mutex::new(Vec::with_capacity(MAX_FAILURES)),
            queue: WorkQueue::default(),
            updates: UpdateSignal::default(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("macflow-capture".into())
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || worker_loop(worker_shared))?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    /// Starts a new UI/discovery generation and drops queued work from the
    /// previous one. Cached thumbnails for still-live windows survive as
    /// immediate stale-while-revalidate previews; dead-window entries are
    /// removed without ever exceeding the byte budget.
    pub fn begin_generation(&self, live_keys: &[WindowKey]) -> u64 {
        // These locks make the transition linear with result insertion. A
        // worker either commits before this point and is filtered below, or
        // observes the new token while holding the same lock and drops work.
        let mut cache = lock(&self.shared.cache);
        let mut failures = lock(&self.shared.failures);
        let generation = self
            .shared
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        retain_live_cache_entries(&mut cache, live_keys);
        failures.clear();
        drop(failures);
        drop(cache);
        // Do not erase a valid batch submitted concurrently just after the
        // atomic generation change.
        self.shared.queue.discard_pending_other_than(generation);
        generation
    }

    #[must_use]
    pub fn mode(&self) -> CaptureMode {
        CaptureMode::from_raw(self.shared.mode.load(Ordering::Acquire))
    }

    /// Changes capture scope and preempts work submitted under the prior mode.
    /// An already-running ScreenCaptureKit call cannot be cancelled, but its
    /// result is rejected by the revision check before entering the cache.
    pub fn set_mode(&self, mode: CaptureMode) {
        let previous = self.shared.mode.swap(mode as u8, Ordering::AcqRel);
        if previous == mode as u8 {
            return;
        }
        self.shared.revision.fetch_add(1, Ordering::AcqRel);
        self.shared.queue.cancel_pending();
        // Every recorded failure belongs to the previous scheduling revision.
        // Dropping those bounded backoffs also prevents a stale deadline from
        // waking the worker after capture has been switched off.
        lock(&self.shared.failures).clear();
        self.shared.updates.publish();
    }

    #[must_use]
    pub fn freshness_policy(&self) -> FreshnessPolicy {
        self.shared.freshness
    }

    /// Installs a coalesced worker-update callback. It runs on the publishing
    /// thread, so an AppKit caller should only enqueue main-thread work from it.
    /// Capture one weak owner in the closure to avoid ownership cycles.
    pub fn set_update_handler(&self, handler: impl Fn() + Send + Sync + 'static) {
        self.shared.updates.set_handler(Arc::new(handler));
    }

    /// Acknowledges the outstanding coalesced update, then returns a current
    /// snapshot. If the worker publishes after acknowledgement, it schedules a
    /// new callback; if it published before, that state is included here.
    #[must_use]
    pub fn acknowledge_update(&self) -> CaptureStats {
        self.shared.updates.acknowledge();
        self.stats()
    }

    /// Invalidates only the supplied cache keys. Other live thumbnails and the
    /// global 8 MiB accounting remain untouched.
    pub fn invalidate_many(&self, keys: &[WindowKey]) -> usize {
        if keys.is_empty() {
            return 0;
        }
        self.shared.revision.fetch_add(1, Ordering::AcqRel);
        self.shared.queue.cancel_pending();
        let removed = {
            let mut cache = lock(&self.shared.cache);
            invalidate_cache_entries(&mut cache, keys)
        };
        lock(&self.shared.failures).retain(|failure| !keys.contains(&failure.target.key));
        self.shared.updates.publish();
        removed
    }

    /// Clears all cached pixels while preserving the configured byte budget.
    pub fn invalidate_all(&self) {
        self.shared.revision.fetch_add(1, Ordering::AcqRel);
        self.shared.queue.cancel_pending();
        lock(&self.shared.cache).clear();
        lock(&self.shared.failures).clear();
        self.shared.updates.publish();
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Acquire)
    }

    /// Submits the selected window first, followed by visible/near-visible
    /// windows. Input is deduplicated and capped; the queue itself has one slot.
    pub fn submit(&self, generation: u64, targets: &[CaptureTarget]) -> SubmitResult {
        if generation != self.generation() {
            return SubmitResult {
                stale_generation: true,
                ..SubmitResult::default()
            };
        }

        let mode = self.mode();
        if mode == CaptureMode::Off {
            return SubmitResult::default();
        }
        let revision = self.shared.revision.load(Ordering::Acquire);

        let now = Instant::now();
        let cache = lock(&self.shared.cache);
        let failures = lock(&self.shared.failures);
        let mut normalized = Vec::with_capacity(targets.len().min(MAX_TARGETS_PER_BATCH));
        let mut coalesced = 0;
        for (index, target) in targets.iter().enumerate() {
            if normalized.len() == MAX_TARGETS_PER_BATCH {
                break;
            }
            let target = CaptureTarget::new(target.key.clone(), target.size);
            let status = thumbnail_status_at(
                cache.peek(&target.key),
                target.size,
                role_for_index(index),
                self.shared.freshness,
                now,
            );
            if target.key.window_id.is_none()
                || !mode_requests_capture(mode, index, status)
                || failures.iter().any(|failure| {
                    failure.generation == generation
                        && failure.revision == revision
                        && failure.target == target
                        && failure.retry_after > now
                })
                || normalized
                    .iter()
                    .any(|item: &CaptureTarget| item.key == target.key)
            {
                coalesced += 1;
                continue;
            }
            normalized.push(target);
        }
        drop(failures);
        drop(cache);

        let mut result = self.shared.queue.submit(
            CaptureBatch {
                generation,
                revision,
                selected_key: targets.first().map(|target| target.key.clone()),
                targets: normalized,
            },
            &self.shared.generation,
            &self.shared.revision,
        );
        // Close the check/submit race with begin_generation. Remove only this
        // stale generation, never a newer batch that won the race.
        if generation != self.generation()
            || revision != self.shared.revision.load(Ordering::Acquire)
        {
            self.shared
                .queue
                .discard_pending_request(generation, revision);
            result.stale_generation = true;
            result.accepted = 0;
        }
        result.coalesced = result.coalesced.saturating_add(coalesced);
        result
    }

    /// Returns a +1 retained image and updates LRU recency.
    /// Stale entries are intentionally returned for immediate presentation;
    /// `submit` independently schedules refresh according to `CaptureMode`.
    pub fn get(&self, key: &WindowKey, requested_size: CaptureSize) -> Option<CFRetained<CGImage>> {
        let requested_size = CaptureSize::bounded(requested_size.width, requested_size.height);
        let mut cache = lock(&self.shared.cache);
        if cache
            .peek(key)
            .is_none_or(|thumbnail| thumbnail.requested_size != requested_size)
        {
            return None;
        }
        cache.get(key).map(|thumbnail| thumbnail.image.clone())
    }

    #[must_use]
    pub fn ready_sequence(&self) -> u64 {
        self.shared.ready_sequence.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn stats(&self) -> CaptureStats {
        let cache = lock(&self.shared.cache);
        let queue = lock(&self.shared.queue.state);
        CaptureStats {
            cache_entries: cache.len(),
            cache_bytes: cache.used_bytes(),
            cache_budget_bytes: cache.budget_bytes(),
            pending_targets: queue
                .pending
                .as_ref()
                .map_or(0, |batch| batch.targets.len()),
            // Covers the gaps before the first per-target marker is installed,
            // between targets, and after the final marker is cleared.
            capture_in_flight: queue.worker_busy,
            ready_sequence: self.ready_sequence(),
            update_pending: self.shared.updates.is_pending(),
            mode: self.mode(),
        }
    }
}

impl Drop for CapturePipeline {
    fn drop(&mut self) {
        self.shared.updates.clear_handler();
        self.shared.queue.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker_loop(shared: Arc<Shared>) {
    loop {
        let batch = match shared
            .queue
            .take_until(next_failure_retry_deadline(&shared))
        {
            QueueWake::Batch(batch) => batch,
            QueueWake::RetryDue => {
                // The capture worker doubles as the single retry scheduler.
                // Expired entries are removed before publishing so the next
                // consumer submission is accepted, and UpdateSignal collapses
                // simultaneous expiries into one outstanding callback.
                if release_due_failures(&shared, Instant::now()) != 0 {
                    shared.updates.publish();
                }
                continue;
            }
            QueueWake::Stopped => break,
        };
        let mode = CaptureMode::from_raw(shared.mode.load(Ordering::Acquire));
        if batch.targets.is_empty()
            || shared.queue.should_preempt()
            || batch.generation != shared.generation.load(Ordering::Acquire)
            || batch.revision != shared.revision.load(Ordering::Acquire)
            || mode == CaptureMode::Off
        {
            shared.queue.finish_batch();
            shared.updates.publish();
            continue;
        }

        // Treat snapshot acquisition as part of the first capture. This keeps
        // duplicate submissions from replacing the selected target while the
        // framework enumerates shareable content. The snapshot retains only
        // the requested windows (at most MAX_TARGETS_PER_BATCH), not the full
        // system-wide SCShareableContent graph.
        let first_target = batch.targets[0].clone();
        shared
            .queue
            .set_in_flight(batch.generation, batch.revision, first_target.clone());
        let context = autoreleasepool(|_| BatchCaptureContext::acquire(&batch.targets));

        for (index, target) in batch.targets.into_iter().enumerate() {
            if index == 0 {
                // A new generation may have arrived during the asynchronous
                // snapshot request. A newer same-generation viewport or
                // shutdown must also preempt before starting a screenshot.
                if shared.queue.should_preempt()
                    || batch.generation != shared.generation.load(Ordering::Acquire)
                    || batch.revision != shared.revision.load(Ordering::Acquire)
                    || CaptureMode::from_raw(shared.mode.load(Ordering::Acquire))
                        == CaptureMode::Off
                {
                    shared
                        .queue
                        .clear_in_flight(batch.generation, batch.revision, &first_target);
                    break;
                }
            } else {
                if shared.queue.should_preempt()
                    || batch.generation != shared.generation.load(Ordering::Acquire)
                    || batch.revision != shared.revision.load(Ordering::Acquire)
                    || CaptureMode::from_raw(shared.mode.load(Ordering::Acquire))
                        == CaptureMode::Off
                {
                    break;
                }
                shared
                    .queue
                    .set_in_flight(batch.generation, batch.revision, target.clone());
            }

            let role = if batch.selected_key.as_ref() == Some(&target.key) {
                CaptureRole::Selected
            } else {
                CaptureRole::Visible
            };
            let current_mode = CaptureMode::from_raw(shared.mode.load(Ordering::Acquire));
            let status = {
                let cache = lock(&shared.cache);
                thumbnail_status_at(
                    cache.peek(&target.key),
                    target.size,
                    role,
                    shared.freshness,
                    Instant::now(),
                )
            };
            let role_index = usize::from(role == CaptureRole::Visible);
            if !mode_requests_capture(current_mode, role_index, status) {
                shared
                    .queue
                    .clear_in_flight(batch.generation, batch.revision, &target);
                continue;
            }

            let captured = autoreleasepool(|_| context.capture(&target));

            match captured {
                Some(image) => {
                    let thumbnail = Thumbnail::new(image, target.size);
                    let cost = thumbnail.cost_bytes();
                    let inserted = {
                        let mut cache = lock(&shared.cache);
                        if batch.generation != shared.generation.load(Ordering::Acquire)
                            || batch.revision != shared.revision.load(Ordering::Acquire)
                            || CaptureMode::from_raw(shared.mode.load(Ordering::Acquire))
                                == CaptureMode::Off
                        {
                            None
                        } else {
                            Some(cache.insert(target.key.clone(), thumbnail, cost))
                        }
                    };
                    match inserted {
                        Some(InsertResult::RejectedOversize) => {
                            record_failure(
                                &shared,
                                batch.generation,
                                batch.revision,
                                target.clone(),
                            );
                        }
                        Some(InsertResult::Inserted | InsertResult::Replaced) => {
                            lock(&shared.failures)
                                .retain(|failure| failure.target.key != target.key);
                            shared.ready_sequence.fetch_add(1, Ordering::Release);
                            shared.updates.publish();
                        }
                        None => {}
                    }
                }
                None => record_failure(&shared, batch.generation, batch.revision, target.clone()),
            }
            // Keep the item marked in-flight until its cache/result publication
            // is complete. A main-thread poll can therefore never observe an
            // idle queue, stop polling, and miss an image committed just after.
            shared
                .queue
                .clear_in_flight(batch.generation, batch.revision, &target);

            // The pending slot contains the newest selection/viewport. Yield
            // to it instead of finishing now-obsolete near-visible work.
            if shared.queue.should_preempt() {
                break;
            }
        }
        shared.queue.finish_batch();
        shared.updates.publish();
    }
}

fn next_failure_retry_deadline(shared: &Shared) -> Option<Instant> {
    let generation = shared.generation.load(Ordering::Acquire);
    let revision = shared.revision.load(Ordering::Acquire);
    let mut failures = lock(&shared.failures);
    // Generation/revision transitions make their old failures irrelevant.
    // Pruning here keeps the fixed-capacity list useful for current work and
    // avoids needless deadline wakes after an invalidation.
    failures.retain(|failure| failure.generation == generation && failure.revision == revision);
    failures.iter().map(|failure| failure.retry_after).min()
}

fn release_due_failures(shared: &Shared, now: Instant) -> usize {
    let generation = shared.generation.load(Ordering::Acquire);
    let revision = shared.revision.load(Ordering::Acquire);
    let mut released = 0;
    lock(&shared.failures).retain(|failure| {
        let is_current = failure.generation == generation && failure.revision == revision;
        if is_current && failure.retry_after <= now {
            released += 1;
            false
        } else {
            is_current
        }
    });
    released
}

fn record_failure(shared: &Shared, generation: u64, revision: u64, target: CaptureTarget) {
    let mut failures = lock(&shared.failures);
    if generation != shared.generation.load(Ordering::Acquire)
        || revision != shared.revision.load(Ordering::Acquire)
    {
        return;
    }
    let retry_after = Instant::now() + FAILURE_BACKOFF;
    if let Some(existing) = failures.iter_mut().find(|failure| failure.target == target) {
        existing.generation = generation;
        existing.revision = revision;
        existing.retry_after = retry_after;
        return;
    }
    if failures.len() == MAX_FAILURES {
        failures.remove(0);
    }
    failures.push(FailedCapture {
        generation,
        revision,
        target,
        retry_after,
    });
}

#[cfg(feature = "modern-capture")]
fn bound_image(image: CFRetained<CGImage>, size: CaptureSize) -> Option<CFRetained<CGImage>> {
    let source_width = CGImage::width(Some(&image));
    let source_height = CGImage::height(Some(&image));
    let max_width = usize::from(size.width);
    let max_height = usize::from(size.height);
    if source_width == 0 || source_height == 0 {
        return None;
    }
    if source_width <= max_width && source_height <= max_height {
        return Some(image);
    }

    let scale =
        (max_width as f64 / source_width as f64).min(max_height as f64 / source_height as f64);
    let width = ((source_width as f64 * scale).round() as usize).clamp(1, max_width);
    let height = ((source_height as f64 * scale).round() as usize).clamp(1, max_height);
    let color_space = CGColorSpace::new_device_rgb()?;
    let context = unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            width,
            height,
            8,
            width.saturating_mul(4),
            Some(&color_space),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }?;
    CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::High);
    CGContext::draw_image(
        Some(&context),
        CGRect::new(CGPoint::ZERO, CGSize::new(width as f64, height as f64)),
        Some(&image),
    );
    CGBitmapContextCreateImage(Some(&context))
}

fn image_cost(image: &CGImage) -> usize {
    CGImage::bytes_per_row(Some(image)).saturating_mul(CGImage::height(Some(image)))
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

fn wait_timeout<'a, T>(
    condition: &Condvar,
    guard: MutexGuard<'a, T>,
    timeout: Duration,
) -> MutexGuard<'a, T> {
    match condition.wait_timeout(guard, timeout) {
        Ok((guard, _)) => guard,
        Err(poisoned) => poisoned.into_inner().0,
    }
}

#[cfg(feature = "modern-capture")]
mod modern {
    use std::ptr::NonNull;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use block2::RcBlock;
    use objc2::AnyThread;
    use objc2::rc::{Retained, autoreleasepool};
    use objc2_core_foundation::CFRetained;
    use objc2_core_graphics::CGImage;
    use objc2_foundation::NSError;
    use objc2_screen_capture_kit::{
        SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration, SCWindow,
    };

    use super::{CaptureTarget, RequestedWindowKey, RequestedWindows};

    const CALLBACK_TIMEOUT: Duration = Duration::from_millis(1_200);
    static CAPTURE_BUSY: AtomicBool = AtomicBool::new(false);

    pub(super) struct Snapshot {
        // Linear lookup avoids another hash allocation at this hard maximum.
        windows: Vec<(RequestedWindowKey, Retained<SCWindow>)>,
    }

    impl Snapshot {
        pub(super) fn acquire(requested: RequestedWindows) -> Option<Self> {
            if requested.is_empty() || !begin_operation() {
                return None;
            }

            let (sender, receiver) = mpsc::sync_channel(1);
            let cancelled = Arc::new(AtomicBool::new(false));
            let callback_cancelled = Arc::clone(&cancelled);
            let content_handler = RcBlock::new(
                move |content: *mut SCShareableContent, error: *mut NSError| {
                    autoreleasepool(|_| {
                        let snapshot = if callback_cancelled.load(Ordering::Acquire)
                            || content.is_null()
                            || !error.is_null()
                        {
                            None
                        } else {
                            // Retain only requested windows while the callback
                            // arguments are valid; the full content graph stays
                            // confined to this autorelease pool.
                            let content = unsafe { &*content };
                            let windows = unsafe { content.windows() };
                            let mut matches = Vec::with_capacity(requested.len());
                            for window in windows.iter() {
                                let window_id = unsafe { window.windowID() };
                                let Some(owner) = (unsafe { window.owningApplication() }) else {
                                    continue;
                                };
                                let key = RequestedWindowKey {
                                    pid: unsafe { owner.processID() },
                                    window_id,
                                };
                                if requested.contains(key) {
                                    matches.push((key, window));
                                    if matches.len() == requested.len() {
                                        break;
                                    }
                                }
                            }
                            Some(Self { windows: matches })
                        };
                        finish_operation();
                        let _ = sender.try_send(snapshot);
                    });
                },
            );

            unsafe {
                SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                    true,
                    false,
                    &content_handler,
                );
            }
            receive_bounded(receiver, cancelled)
        }

        pub(super) fn capture(&self, target: &CaptureTarget) -> Option<CFRetained<CGImage>> {
            let window_id = target.key.window_id?;
            let requested = RequestedWindowKey {
                pid: target.key.pid,
                window_id,
            };
            let window = self
                .windows
                .iter()
                .find(|(candidate, _)| *candidate == requested)
                .map(|(_, window)| window)?;
            if !begin_operation() {
                return None;
            }

            let size = target.size;
            let filter = unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window)
            };
            let configuration = unsafe { SCStreamConfiguration::new() };
            unsafe {
                configuration.setWidth(usize::from(size.width));
                configuration.setHeight(usize::from(size.height));
                configuration.setQueueDepth(1);
                configuration.setShowsCursor(false);
                configuration.setScalesToFit(true);
                configuration.setPreservesAspectRatio(true);
                configuration.setIgnoreShadowsSingleWindow(true);
                configuration.setIgnoreGlobalClipSingleWindow(true);
            }

            // ScreenCaptureKit exposes no cancellation handle. Keep the
            // global permit until this callback arrives, including after a
            // timeout, so framework operations can never accumulate.
            let (sender, receiver) = mpsc::sync_channel(1);
            let cancelled = Arc::new(AtomicBool::new(false));
            let callback_cancelled = Arc::clone(&cancelled);
            let image_handler = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
                autoreleasepool(|_| {
                    let image = if !callback_cancelled.load(Ordering::Acquire) && error.is_null() {
                        NonNull::new(image)
                            .map(|image| unsafe { CFRetained::<CGImage>::retain(image) })
                    } else {
                        None
                    };
                    finish_operation();
                    let _ = sender.try_send(image);
                });
            });

            unsafe {
                SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                    &filter,
                    &configuration,
                    Some(&image_handler),
                );
            }
            receive_bounded(receiver, cancelled)
        }
    }

    fn begin_operation() -> bool {
        CAPTURE_BUSY
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn finish_operation() {
        CAPTURE_BUSY.store(false, Ordering::Release);
    }

    fn receive_bounded<T>(
        receiver: mpsc::Receiver<Option<T>>,
        cancelled: Arc<AtomicBool>,
    ) -> Option<T> {
        match receiver.recv_timeout(CALLBACK_TIMEOUT) {
            Ok(value) => value,
            Err(_) => {
                cancelled.store(true, Ordering::Release);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    fn key(value: u32) -> WindowKey {
        WindowKey {
            pid: (value % 47) as i32,
            window_id: Some(value),
            fallback_ordinal: 0,
        }
    }

    fn target(value: u32) -> CaptureTarget {
        CaptureTarget::new(key(value), CaptureSize::default())
    }

    fn sized_target(value: u32, width: u16, height: u16) -> CaptureTarget {
        CaptureTarget::new(key(value), CaptureSize::bounded(width, height))
    }

    fn batch(generation: u64, targets: Vec<CaptureTarget>) -> CaptureBatch {
        CaptureBatch {
            generation,
            revision: 0,
            selected_key: targets.first().map(|target| target.key.clone()),
            targets,
        }
    }

    static TEST_REVISION: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn capture_size_preserves_wide_tall_and_compact_window_aspects() {
        assert_eq!(
            CaptureSize::preserving_aspect_ratio(1_600.0, 900.0),
            CaptureSize::bounded(560, 315)
        );
        assert_eq!(
            CaptureSize::preserving_aspect_ratio(400.0, 800.0),
            CaptureSize::bounded(168, 336)
        );
        // A physically small window still gets enough capture resolution for
        // a sharp preview, but its canvas retains the window's 2:1 ratio.
        assert_eq!(
            CaptureSize::preserving_aspect_ratio(240.0, 120.0),
            CaptureSize::bounded(560, 280)
        );
    }

    #[test]
    fn capture_size_falls_back_safely_for_invalid_bounds() {
        for (width, height) in [
            (0.0, 100.0),
            (100.0, 0.0),
            (-1.0, 100.0),
            (f64::NAN, 100.0),
            (100.0, f64::INFINITY),
        ] {
            assert_eq!(
                CaptureSize::preserving_aspect_ratio(width, height),
                CaptureSize::default()
            );
        }
    }

    #[test]
    fn batch_snapshot_request_is_fixed_capacity_and_deduplicated() {
        let mut targets: Vec<_> = (1..=64).map(target).collect();
        targets.insert(0, target(1));
        targets.insert(
            1,
            CaptureTarget::new(
                WindowKey {
                    pid: 99,
                    window_id: None,
                    fallback_ordinal: 0,
                },
                CaptureSize::default(),
            ),
        );

        let requested = RequestedWindows::from_targets(&targets);
        assert_eq!(requested.len(), MAX_TARGETS_PER_BATCH);
        assert!(requested.contains(RequestedWindowKey {
            pid: 1,
            window_id: 1,
        }));
        assert!(requested.contains(RequestedWindowKey {
            pid: (MAX_TARGETS_PER_BATCH as u32 % 47) as i32,
            window_id: MAX_TARGETS_PER_BATCH as u32,
        }));
        assert!(!requested.contains(RequestedWindowKey {
            pid: ((MAX_TARGETS_PER_BATCH + 1) as u32 % 47) as i32,
            window_id: (MAX_TARGETS_PER_BATCH + 1) as u32,
        }));
    }

    #[test]
    fn snapshot_request_keys_window_ids_by_owning_process() {
        let targets = [
            CaptureTarget::new(
                WindowKey {
                    pid: 7,
                    window_id: Some(42),
                    fallback_ordinal: 0,
                },
                CaptureSize::default(),
            ),
            CaptureTarget::new(
                WindowKey {
                    pid: 8,
                    window_id: Some(42),
                    fallback_ordinal: 0,
                },
                CaptureSize::default(),
            ),
            CaptureTarget::new(
                WindowKey {
                    pid: 7,
                    window_id: Some(42),
                    fallback_ordinal: 9,
                },
                CaptureSize::default(),
            ),
        ];

        let requested = RequestedWindows::from_targets(&targets);
        assert_eq!(requested.len(), 2);
        assert!(requested.contains(RequestedWindowKey {
            pid: 7,
            window_id: 42,
        }));
        assert!(requested.contains(RequestedWindowKey {
            pid: 8,
            window_id: 42,
        }));
        assert!(!requested.contains(RequestedWindowKey {
            pid: 9,
            window_id: 42,
        }));
    }

    #[test]
    fn queue_is_capacity_one_and_deduplicates() {
        let queue = WorkQueue::default();
        let generation = AtomicU64::new(1);
        let result = queue.submit(
            batch(1, vec![target(1), target(1), target(2)]),
            &generation,
            &TEST_REVISION,
        );
        assert_eq!(result.accepted, 2);
        assert_eq!(result.coalesced, 1);

        let result = queue.submit(
            batch(1, (2..40).map(target).collect()),
            &generation,
            &TEST_REVISION,
        );
        assert!(result.replaced_pending_batch);
        let state = lock(&queue.state);
        assert_eq!(state.pending.as_ref().map(|_| 1), Some(1));
        assert!(
            state
                .pending
                .as_ref()
                .is_some_and(|batch| batch.targets.len() <= MAX_TARGETS_PER_BATCH)
        );
    }

    #[test]
    fn newer_size_replaces_stale_pending_size_for_the_same_window() {
        let queue = WorkQueue::default();
        let generation = AtomicU64::new(1);
        let stale = sized_target(7, 560, 336);
        let current = sized_target(7, 168, 336);
        queue.submit(batch(1, vec![stale]), &generation, &TEST_REVISION);
        let result = queue.submit(batch(1, vec![current.clone()]), &generation, &TEST_REVISION);

        assert!(result.replaced_pending_batch);
        assert_eq!(result.coalesced, 1);
        let state = lock(&queue.state);
        assert_eq!(
            state
                .pending
                .as_ref()
                .and_then(|batch| batch.targets.first()),
            Some(&current)
        );
        assert_eq!(
            state.pending.as_ref().map(|batch| batch.targets.len()),
            Some(1)
        );
    }

    #[test]
    fn in_flight_deduplication_includes_requested_size() {
        let queue = WorkQueue::default();
        let generation = AtomicU64::new(3);
        let active = sized_target(9, 560, 336);
        queue.set_in_flight(3, 0, active.clone());

        let duplicate = queue.submit(batch(3, vec![active]), &generation, &TEST_REVISION);
        assert_eq!(duplicate.accepted, 0);
        assert_eq!(duplicate.coalesced, 1);

        let resized = sized_target(9, 224, 336);
        let changed = queue.submit(batch(3, vec![resized.clone()]), &generation, &TEST_REVISION);
        assert_eq!(changed.accepted, 1);
        assert_eq!(
            lock(&queue.state)
                .pending
                .as_ref()
                .and_then(|batch| batch.targets.first()),
            Some(&resized)
        );
    }

    #[test]
    fn current_in_flight_size_cancels_an_older_pending_resize() {
        let queue = WorkQueue::default();
        let generation = AtomicU64::new(5);
        let current = sized_target(11, 560, 315);
        let stale_resize = sized_target(11, 224, 336);
        queue.set_in_flight(5, 0, current.clone());
        queue.submit(batch(5, vec![stale_resize]), &generation, &TEST_REVISION);

        let result = queue.submit(batch(5, vec![current]), &generation, &TEST_REVISION);
        assert!(result.replaced_pending_batch);
        assert_eq!(result.accepted, 0);
        assert_eq!(result.coalesced, 2);
        assert!(lock(&queue.state).pending.is_none());
    }

    #[test]
    fn concurrent_submission_never_grows_the_queue() {
        let queue = Arc::new(WorkQueue::default());
        let generation = Arc::new(AtomicU64::new(9));
        let revision = Arc::new(AtomicU64::new(0));
        let mut producers = Vec::new();
        for producer in 0..8_u32 {
            let queue = Arc::clone(&queue);
            let generation = Arc::clone(&generation);
            let revision = Arc::clone(&revision);
            producers.push(thread::spawn(move || {
                for sequence in 0..25_000_u32 {
                    let base = producer * 10_000 + sequence;
                    queue.submit(
                        batch(9, vec![target(base), target(base), target(base + 1)]),
                        &generation,
                        &revision,
                    );
                    let state = lock(&queue.state);
                    assert!(state.pending.iter().count() <= 1);
                    assert!(
                        state
                            .pending
                            .as_ref()
                            .is_none_or(|batch| batch.targets.len() <= MAX_TARGETS_PER_BATCH)
                    );
                }
            }));
        }
        for producer in producers {
            producer.join().unwrap();
        }
    }

    #[test]
    fn stale_generation_work_is_cancelled() {
        let queue = WorkQueue::default();
        let generation = AtomicU64::new(10);
        queue.submit(
            batch(10, vec![target(1), target(2)]),
            &generation,
            &TEST_REVISION,
        );
        queue.cancel_pending();
        assert!(lock(&queue.state).pending.is_none());
    }

    #[test]
    fn stale_producer_cannot_replace_newer_batch() {
        let queue = WorkQueue::default();
        let generation = AtomicU64::new(11);
        queue.submit(batch(11, vec![target(11)]), &generation, &TEST_REVISION);
        let stale = queue.submit(batch(10, vec![target(10)]), &generation, &TEST_REVISION);
        assert!(stale.stale_generation);
        assert_eq!(
            lock(&queue.state)
                .pending
                .as_ref()
                .and_then(|batch| batch.targets.first())
                .map(|target| target.key.window_id),
            Some(Some(11))
        );
    }

    #[test]
    fn newer_viewport_preempts_after_snapshot_before_first_capture() {
        let queue = WorkQueue::default();
        let generation = AtomicU64::new(7);
        queue.submit(batch(7, vec![target(1)]), &generation, &TEST_REVISION);
        let active = queue.take().expect("initial batch");
        queue.set_in_flight(
            active.generation,
            active.revision,
            active.targets[0].clone(),
        );
        queue.submit(batch(7, vec![target(2)]), &generation, &TEST_REVISION);
        assert!(queue.should_preempt());
    }

    #[test]
    fn adaptive_modes_apply_distinct_selected_and_visible_policy() {
        for status in [ThumbnailStatus::Missing, ThumbnailStatus::Stale] {
            assert!(!mode_requests_capture(CaptureMode::Off, 0, status));
            assert!(mode_requests_capture(CaptureMode::Selected, 0, status));
            assert!(!mode_requests_capture(CaptureMode::Selected, 1, status));
            assert!(mode_requests_capture(CaptureMode::Fresh, 0, status));
            assert!(mode_requests_capture(CaptureMode::Fresh, 1, status));
        }
        assert!(mode_requests_capture(
            CaptureMode::Visible,
            0,
            ThumbnailStatus::Stale
        ));
        assert!(!mode_requests_capture(
            CaptureMode::Visible,
            1,
            ThumbnailStatus::Stale
        ));
        assert!(mode_requests_capture(
            CaptureMode::Visible,
            1,
            ThumbnailStatus::Missing
        ));
        for mode in [
            CaptureMode::Selected,
            CaptureMode::Visible,
            CaptureMode::Fresh,
        ] {
            assert!(!mode_requests_capture(mode, 0, ThumbnailStatus::Fresh));
            assert!(!mode_requests_capture(mode, 1, ThumbnailStatus::Fresh));
        }
    }

    #[test]
    fn selected_and_visible_expiry_are_independent_and_size_sensitive() {
        let policy = FreshnessPolicy::default();
        let now = Instant::now();
        let size = CaptureSize::bounded(560, 315);
        let captured_at = now - Duration::from_millis(700);

        assert_eq!(
            thumbnail_status_from_metadata(
                Some((size, captured_at)),
                size,
                CaptureRole::Selected,
                policy,
                now,
            ),
            ThumbnailStatus::Stale
        );
        assert_eq!(
            thumbnail_status_from_metadata(
                Some((size, captured_at)),
                size,
                CaptureRole::Visible,
                policy,
                now,
            ),
            ThumbnailStatus::Fresh
        );
        assert_eq!(
            thumbnail_status_from_metadata(
                Some((size, now)),
                CaptureSize::bounded(168, 336),
                CaptureRole::Selected,
                policy,
                now,
            ),
            ThumbnailStatus::Missing
        );
        assert_eq!(
            thumbnail_status_from_metadata(None, size, CaptureRole::Visible, policy, now),
            ThumbnailStatus::Missing
        );
    }

    #[test]
    fn pathological_zero_expiry_is_normalized_to_bounded_refresh_intervals() {
        let policy = FreshnessPolicy {
            selected_max_age: Duration::ZERO,
            visible_max_age: Duration::ZERO,
        }
        .normalized();
        assert_eq!(
            policy.selected_max_age,
            FreshnessPolicy::MIN_SELECTED_MAX_AGE
        );
        assert_eq!(policy.visible_max_age, FreshnessPolicy::MIN_VISIBLE_MAX_AGE);
    }

    #[test]
    fn live_generation_retention_and_targeted_invalidation_preserve_budget() {
        let mut cache = TinyLru::new(100);
        cache.insert(key(1), 10_u32, 25);
        cache.insert(key(2), 20_u32, 25);
        cache.insert(key(3), 30_u32, 25);

        retain_live_cache_entries(&mut cache, &[key(2), key(3), key(99)]);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.used_bytes(), 50);
        assert_eq!(cache.peek(&key(2)), Some(&20));
        assert_eq!(cache.peek(&key(3)), Some(&30));

        assert_eq!(invalidate_cache_entries(&mut cache, &[key(3)]), 1);
        assert_eq!(invalidate_cache_entries(&mut cache, &[key(3)]), 0);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.used_bytes(), 25);
        assert!(cache.used_bytes() <= cache.budget_bytes());
    }

    #[test]
    fn update_notifications_coalesce_until_acknowledged() {
        let signal = UpdateSignal::default();
        let callbacks = Arc::new(AtomicU64::new(0));
        let callback_count = Arc::clone(&callbacks);
        signal.set_handler(Arc::new(move || {
            callback_count.fetch_add(1, Ordering::Relaxed);
        }));

        for _ in 0..250_000 {
            signal.publish();
        }
        assert!(signal.is_pending());
        assert_eq!(callbacks.load(Ordering::Relaxed), 1);

        signal.acknowledge();
        assert!(!signal.is_pending());
        signal.publish();
        assert_eq!(callbacks.load(Ordering::Relaxed), 2);

        signal.clear_handler();
        signal.acknowledge();
        signal.publish();
        assert_eq!(callbacks.load(Ordering::Relaxed), 2);
        let replacement_count = Arc::clone(&callbacks);
        signal.set_handler(Arc::new(move || {
            replacement_count.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(callbacks.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn concurrent_update_publishers_still_issue_one_callback() {
        let signal = Arc::new(UpdateSignal::default());
        let callbacks = Arc::new(AtomicU64::new(0));
        let callback_count = Arc::clone(&callbacks);
        signal.set_handler(Arc::new(move || {
            callback_count.fetch_add(1, Ordering::Relaxed);
        }));

        let mut publishers = Vec::new();
        for _ in 0..8 {
            let signal = Arc::clone(&signal);
            publishers.push(thread::spawn(move || {
                for _ in 0..25_000 {
                    signal.publish();
                }
            }));
        }
        for publisher in publishers {
            publisher.join().unwrap();
        }
        assert!(signal.is_pending());
        assert_eq!(callbacks.load(Ordering::Relaxed), 1);

        signal.acknowledge();
        signal.publish();
        assert_eq!(callbacks.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn revision_change_rejects_stale_work_without_coalescing_new_refresh() {
        let queue = WorkQueue::default();
        let generation = AtomicU64::new(4);
        let revision = AtomicU64::new(1);
        let mut stale = batch(4, vec![target(8)]);
        stale.revision = 0;
        assert!(queue.submit(stale, &generation, &revision).stale_generation);

        let mut current = batch(4, vec![target(8)]);
        current.revision = 1;
        assert_eq!(queue.submit(current, &generation, &revision).accepted, 1);
        let active = queue.take().expect("current revision batch");
        queue.set_in_flight(
            active.generation,
            active.revision,
            active.targets[0].clone(),
        );

        revision.store(2, Ordering::Release);
        let mut refreshed = batch(4, vec![target(8)]);
        refreshed.revision = 2;
        let result = queue.submit(refreshed, &generation, &revision);
        assert_eq!(result.accepted, 1);
        assert_eq!(result.coalesced, 0);
    }

    #[test]
    fn retry_deadline_wait_prioritizes_new_capture_work() {
        let queue = WorkQueue::default();
        let generation = AtomicU64::new(6);
        assert_eq!(
            queue
                .submit(batch(6, vec![target(42)]), &generation, &TEST_REVISION,)
                .accepted,
            1
        );

        // Even an already-expired retry deadline cannot delay a newly selected
        // window; queued capture work always wins the single worker slot.
        match queue.take_until(Some(Instant::now())) {
            QueueWake::Batch(batch) => assert_eq!(batch.targets, vec![target(42)]),
            QueueWake::RetryDue | QueueWake::Stopped => panic!("capture batch was not prioritized"),
        }
    }

    #[test]
    fn retry_deadline_wakes_the_queue_without_polling() {
        let queue = WorkQueue::default();
        let started = Instant::now();
        let deadline = started + Duration::from_millis(15);
        assert!(matches!(
            queue.take_until(Some(deadline)),
            QueueWake::RetryDue
        ));
        assert!(Instant::now() >= deadline);
    }

    #[test]
    fn due_failures_publish_one_coalesced_retry_update() {
        let shared = Shared {
            generation: AtomicU64::new(12),
            revision: AtomicU64::new(3),
            ready_sequence: AtomicU64::new(0),
            mode: AtomicU8::new(CaptureMode::Fresh as u8),
            freshness: FreshnessPolicy::default(),
            cache: Mutex::new(TinyLru::new(1)),
            failures: Mutex::new(vec![
                FailedCapture {
                    generation: 12,
                    revision: 3,
                    target: target(1),
                    retry_after: Instant::now() - Duration::from_millis(1),
                },
                FailedCapture {
                    generation: 12,
                    revision: 3,
                    target: target(2),
                    retry_after: Instant::now() - Duration::from_millis(1),
                },
                // Obsolete failures are pruned and never schedule callbacks.
                FailedCapture {
                    generation: 12,
                    revision: 2,
                    target: target(3),
                    retry_after: Instant::now() - Duration::from_millis(1),
                },
                FailedCapture {
                    generation: 12,
                    revision: 3,
                    target: target(4),
                    retry_after: Instant::now() + Duration::from_secs(60),
                },
            ]),
            queue: WorkQueue::default(),
            updates: UpdateSignal::default(),
        };
        let callbacks = Arc::new(AtomicU64::new(0));
        let callback_count = Arc::clone(&callbacks);
        shared.updates.set_handler(Arc::new(move || {
            callback_count.fetch_add(1, Ordering::Relaxed);
        }));

        // Selecting the next deadline prunes the obsolete revision first.
        assert!(next_failure_retry_deadline(&shared).is_some());
        assert_eq!(lock(&shared.failures).len(), 3);
        assert_eq!(release_due_failures(&shared, Instant::now()), 2);
        shared.updates.publish();
        shared.updates.publish();

        assert_eq!(callbacks.load(Ordering::Relaxed), 1);
        assert_eq!(lock(&shared.failures).len(), 1);
        assert!(shared.updates.is_pending());
    }

    #[test]
    fn failure_backoff_storage_stays_fixed_capacity_under_stress() {
        let shared = Shared {
            generation: AtomicU64::new(15),
            revision: AtomicU64::new(4),
            ready_sequence: AtomicU64::new(0),
            mode: AtomicU8::new(CaptureMode::Fresh as u8),
            freshness: FreshnessPolicy::default(),
            cache: Mutex::new(TinyLru::new(1)),
            failures: Mutex::new(Vec::with_capacity(MAX_FAILURES)),
            queue: WorkQueue::default(),
            updates: UpdateSignal::default(),
        };

        for value in 0..250_000_u32 {
            record_failure(&shared, 15, 4, target(value));
            assert!(lock(&shared.failures).len() <= MAX_FAILURES);
        }
        assert_eq!(lock(&shared.failures).len(), MAX_FAILURES);
        assert!(next_failure_retry_deadline(&shared).is_some());
    }

    #[test]
    fn byte_cache_stress_stays_strictly_bounded() {
        let budget = DEFAULT_CACHE_BUDGET_BYTES;
        let mut cache = TinyLru::new(budget);
        let mut random = 0xA341_316C_u32;
        for _ in 0..250_000 {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            let cost = 4 * ((random as usize % 200_000) + 1);
            cache.insert(key(random % 128), random, cost);
            if random & 7 == 0 {
                let _ = cache.get(&key(random % 128));
            }
            assert!(cache.used_bytes() <= cache.budget_bytes());
        }
    }

    #[test]
    fn pipeline_generation_and_shutdown_stress() {
        let pipeline = CapturePipeline::with_budget(64 * 1024).unwrap();
        for expected in 1..=100_000_u64 {
            assert_eq!(pipeline.begin_generation(&[]), expected);
            assert_eq!(pipeline.generation(), expected);
        }
        assert_eq!(pipeline.stats().cache_bytes, 0);
        // Drop joins the single sleeping worker; this also exercises the
        // condition-variable shutdown path under repeated generation churn.
        drop(pipeline);
    }

    #[test]
    fn pipeline_mode_and_invalidation_updates_remain_coalesced_and_bounded() {
        let pipeline = CapturePipeline::with_budget_and_freshness(
            64 * 1024,
            FreshnessPolicy {
                selected_max_age: Duration::ZERO,
                visible_max_age: Duration::ZERO,
            },
        )
        .unwrap();
        assert_eq!(
            pipeline.freshness_policy(),
            FreshnessPolicy {
                selected_max_age: FreshnessPolicy::MIN_SELECTED_MAX_AGE,
                visible_max_age: FreshnessPolicy::MIN_VISIBLE_MAX_AGE,
            }
        );

        let callbacks = Arc::new(AtomicU64::new(0));
        let callback_count = Arc::clone(&callbacks);
        pipeline.set_update_handler(move || {
            callback_count.fetch_add(1, Ordering::Relaxed);
        });
        pipeline.set_mode(CaptureMode::Selected);
        assert_eq!(callbacks.load(Ordering::Relaxed), 1);
        let stats = pipeline.acknowledge_update();
        assert_eq!(stats.mode, CaptureMode::Selected);
        assert!(!stats.update_pending);

        for value in 0..100_000_u32 {
            assert_eq!(pipeline.invalidate_many(&[key(value % 97)]), 0);
        }
        assert_eq!(callbacks.load(Ordering::Relaxed), 2);
        let stats = pipeline.acknowledge_update();
        assert_eq!(stats.cache_bytes, 0);
        assert!(stats.cache_bytes <= stats.cache_budget_bytes);
        assert_eq!(stats.pending_targets, 0);
        drop(pipeline);
    }
}
