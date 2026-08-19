use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(not(feature = "app-store"))]
use std::ffi::{CStr, c_void};
#[cfg(not(feature = "app-store"))]
use std::sync::OnceLock;

use dispatch2::{DispatchQueue, DispatchTime};
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};
use objc2_application_services::{
    AXCopyMultipleAttributeOptions, AXError, AXUIElement, AXValue, AXValueType,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint, CGRect,
    CGSize,
};
use objc2_core_graphics::{
    CGRectMakeWithDictionaryRepresentation, CGWindowID, CGWindowListCopyWindowInfo,
    CGWindowListOption, kCGWindowBounds, kCGWindowIsOnscreen, kCGWindowLayer, kCGWindowName,
    kCGWindowNumber, kCGWindowOwnerPID,
};
#[cfg(not(feature = "app-store"))]
use objc2_foundation::NSNotificationCenter;
#[cfg(not(feature = "app-store"))]
use objc2_foundation::NSProcessInfo;
#[cfg(not(feature = "app-store"))]
use objc2_foundation::NSString;

use crate::core::model::{
    Bounds, ProcessId, WindowId, WindowIdentity, WindowKey, WindowMetadata, WindowOrder,
};

const MATCH_DISTANCE_LIMIT: f64 = 120.0;
const MATCH_AMBIGUITY_EPSILON: f64 = 1.0;
const FIRST_FOCUS_REPAIR_DELAY: Duration = Duration::from_millis(120);
const FINAL_FOCUS_REPAIR_DELAY: Duration = Duration::from_millis(360);
const SYNTHETIC_FOCUS_BUDGET: Duration = Duration::from_millis(160);
const MAX_SYNTHETIC_TITLE_SCAN: usize = 8;
const FOCUS_AX_TIMEOUT_SECONDS: f32 = 0.08;
const MAX_CG_ENTRIES: usize = 2_048;
const MAX_AX_WINDOWS_PER_APP: usize = 128;
const MAX_DISCOVERED_WINDOWS: usize = 512;
const MAX_CONSECUTIVE_AX_FAILURES: usize = 4;
const DISCOVERY_AX_TIMEOUT_SECONDS: f32 = 0.15;
const DISCOVERY_BUDGET: Duration = Duration::from_millis(900);

static FOCUS_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(not(feature = "app-store"))]
type CgsConnectionId = u32;
#[cfg(not(feature = "app-store"))]
type CgsDefaultConnection = unsafe extern "C" fn() -> CgsConnectionId;
#[cfg(not(feature = "app-store"))]
type CgsCopySpacesForWindows =
    unsafe extern "C" fn(CgsConnectionId, isize, *const CFType) -> *const CFType;
#[cfg(not(feature = "app-store"))]
type CgsShowSpaces = unsafe extern "C" fn(CgsConnectionId, *const CFType);
#[cfg(not(feature = "app-store"))]
#[repr(C)]
struct ProcessSerialNumber {
    high: u32,
    low: u32,
}
#[cfg(not(feature = "app-store"))]
type GetProcessForPid = unsafe extern "C" fn(ProcessId, *mut ProcessSerialNumber) -> i32;
#[cfg(not(feature = "app-store"))]
type SlpsSetFrontProcessWithOptions =
    unsafe extern "C" fn(*mut ProcessSerialNumber, WindowId, u32) -> i32;
#[cfg(not(feature = "app-store"))]
type SlpsPostEventRecordTo = unsafe extern "C" fn(*mut ProcessSerialNumber, *mut u8) -> i32;

/// Optional private Space-switching entry points. They are resolved lazily,
/// every symbol remains nullable, and the framework handle intentionally
/// stays open for the life of the process so no copied function pointer can
/// outlive its image.
#[cfg(not(feature = "app-store"))]
struct SkyLightApi {
    _handle: usize,
    default_connection: Option<CgsDefaultConnection>,
    copy_spaces_for_windows: Option<CgsCopySpacesForWindows>,
    show_spaces: Option<CgsShowSpaces>,
    get_process_for_pid: Option<GetProcessForPid>,
    set_front_process: Option<SlpsSetFrontProcessWithOptions>,
    post_event_record: Option<SlpsPostEventRecordTo>,
}

#[cfg(not(feature = "app-store"))]
impl SkyLightApi {
    fn load() -> Self {
        // SAFETY: The path is a statically allocated C string.
        let handle = unsafe {
            libc::dlopen(
                c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight".as_ptr(),
                libc::RTLD_LAZY | libc::RTLD_LOCAL,
            )
        };
        if handle.is_null() {
            return Self {
                _handle: 0,
                default_connection: None,
                copy_spaces_for_windows: None,
                show_spaces: None,
                get_process_for_pid: None,
                set_front_process: None,
                post_event_record: None,
            };
        }

        // Prefer current symbol spellings while retaining the aliases used by
        // older macOS releases. Each lookup remains independently nullable.
        let connection_symbol = resolve_first_symbol(
            handle,
            &[
                c"CGSMainConnectionID",
                c"SLSMainConnectionID",
                c"CGSDefaultConnection",
            ],
        );
        let copy_spaces_symbol = resolve_first_symbol(
            handle,
            &[c"SLSCopySpacesForWindows", c"CGSCopySpacesForWindows"],
        );
        let show_spaces_symbol =
            resolve_first_symbol(handle, &[c"SLSShowSpaces", c"CGSShowSpaces"]);
        let set_front_process_symbol =
            resolve_first_symbol(handle, &[c"_SLPSSetFrontProcessWithOptions"]);
        let post_event_record_symbol = resolve_first_symbol(handle, &[c"SLPSPostEventRecordTo"]);
        // GetProcessForPID lives in the already-loaded ApplicationServices
        // image rather than SkyLight itself.
        let get_process_for_pid_symbol =
            unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"GetProcessForPID".as_ptr()) };

        // SAFETY: dlsym returns either null or an address for the exact C ABI
        // symbols named above. Calls are still guarded by each Option.
        let default_connection = unsafe {
            (!connection_symbol.is_null()).then(|| {
                std::mem::transmute::<*mut c_void, CgsDefaultConnection>(connection_symbol)
            })
        };
        // SAFETY: Same reasoning as `default_connection`; this is the known
        // SkyLight signature and is never called unless all symbols resolved.
        let copy_spaces_for_windows = unsafe {
            (!copy_spaces_symbol.is_null()).then(|| {
                std::mem::transmute::<*mut c_void, CgsCopySpacesForWindows>(copy_spaces_symbol)
            })
        };
        // SAFETY: Same reasoning as `default_connection`.
        let show_spaces = unsafe {
            (!show_spaces_symbol.is_null())
                .then(|| std::mem::transmute::<*mut c_void, CgsShowSpaces>(show_spaces_symbol))
        };
        // SAFETY: This is the stable Carbon C ABI: pid plus an output PSN.
        let get_process_for_pid = unsafe {
            (!get_process_for_pid_symbol.is_null()).then(|| {
                std::mem::transmute::<*mut c_void, GetProcessForPid>(get_process_for_pid_symbol)
            })
        };
        // SAFETY: Same reasoning as the other lazily resolved SkyLight calls.
        // Both signatures are verified against the current macOS 14-26 ABI.
        let set_front_process = unsafe {
            (!set_front_process_symbol.is_null()).then(|| {
                std::mem::transmute::<*mut c_void, SlpsSetFrontProcessWithOptions>(
                    set_front_process_symbol,
                )
            })
        };
        // SAFETY: The event-record call consumes a pointer only for the call's
        // duration; `make_key_window` keeps its fixed buffer alive throughout.
        let post_event_record = unsafe {
            (!post_event_record_symbol.is_null()).then(|| {
                std::mem::transmute::<*mut c_void, SlpsPostEventRecordTo>(post_event_record_symbol)
            })
        };

        Self {
            _handle: handle as usize,
            default_connection,
            copy_spaces_for_windows,
            show_spaces,
            get_process_for_pid,
            set_front_process,
            post_event_record,
        }
    }
}

#[cfg(not(feature = "app-store"))]
fn resolve_first_symbol(handle: *mut c_void, names: &[&CStr]) -> *mut c_void {
    for name in names {
        // SAFETY: `handle` is the live result of dlopen and every name is a
        // validated CStr. The handle intentionally remains loaded forever.
        let symbol = unsafe { libc::dlsym(handle, name.as_ptr()) };
        if !symbol.is_null() {
            return symbol;
        }
    }
    ptr::null_mut()
}

#[cfg(not(feature = "app-store"))]
fn skylight() -> &'static SkyLightApi {
    static API: OnceLock<SkyLightApi> = OnceLock::new();
    API.get_or_init(SkyLightApi::load)
}

#[derive(Clone, Debug)]
pub struct AppSeed {
    pub pid: ProcessId,
    pub name: Box<str>,
    pub bundle_identifier: Option<Box<str>>,
}

#[derive(Clone, Debug)]
pub struct DisplayGeometry {
    pub frame: Bounds,
    // Preserved in the discovery snapshot for future display-specific policy;
    // coordinate matching intentionally does not use it for a vertical flip.
    #[allow(dead_code)]
    pub is_main: bool,
}

#[derive(Clone)]
pub struct AxHandle(CFRetained<AXUIElement>);

// AXUIElement is a retained cross-process proxy. MacFlow transfers each
// proxy from its single discovery worker to the main thread exactly once and
// never accesses it concurrently. The bindings conservatively omit Send.
unsafe impl Send for AxHandle {}

impl std::fmt::Debug for AxHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("AxHandle")
            .field(&CFRetained::as_ptr(&self.0))
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct ManagedWindow {
    pub metadata: WindowMetadata,
    pub ax_element: AxHandle,
}

#[derive(Debug)]
pub struct DiscoveryResult {
    pub windows: Vec<ManagedWindow>,
    pub z_order: Vec<WindowOrder>,
}

/// Minimal, bounded WindowServer snapshot used for liveness pruning and exact
/// quick switching. It deliberately carries no titles, bounds, AX elements, or
/// thumbnails, so taking one does not trigger cross-process discovery.
#[derive(Clone, Debug)]
pub struct LiveWindowSnapshot {
    identities: Vec<WindowIdentity>,
}

impl LiveWindowSnapshot {
    #[must_use]
    pub fn identities(&self) -> &[WindowIdentity] {
        &self.identities
    }
}

/// Result of the fail-closed, no-discovery quick-focus path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExactQuickSwitchResult {
    /// The exact process/window pair was fronted and made key.
    Focused,
    /// The key has no stable WindowServer ID and requires normal AX discovery.
    InvalidKey,
    /// WindowServer could not provide a validation snapshot.
    SnapshotUnavailable,
    /// The pair was absent from a fresh snapshot and must be pruned as closed.
    WindowClosed,
    /// The pair is live, but exact private focus is unavailable or rejected.
    RequiresFullFocus,
}

/// Result of the lightweight focused-window sampler used by live tracking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveFocusIdentity {
    Focused(WindowIdentity),
    NoWindow,
    SnapshotUnavailable,
}

#[derive(Clone, Debug)]
struct CgEntry {
    window_id: WindowId,
    bounds: Bounds,
    title: Box<str>,
    // `kCGWindowIsOnscreen` is optional. Missing must remain unknown: treating
    // it as true can discard a fullscreen window on another display/Space as
    // a geometry duplicate of the current AX window.
    onscreen: Option<bool>,
    z_index: usize,
}

#[derive(Debug)]
struct AxNames {
    windows: CFRetained<CFString>,
    focused_window: CFRetained<CFString>,
    title: CFRetained<CFString>,
    window_attributes: CFRetained<CFArray<CFString>>,
}

impl AxNames {
    fn new() -> Self {
        let windows = CFString::from_static_str("AXWindows");
        let focused_window = CFString::from_static_str("AXFocusedWindow");
        let title = CFString::from_static_str("AXTitle");
        let minimized = CFString::from_static_str("AXMinimized");
        let position = CFString::from_static_str("AXPosition");
        let size = CFString::from_static_str("AXSize");
        // Some AX implementations expose the backing Core Graphics window ID
        // directly. Keep it in the batched request as the strongest available
        // identity signal; unsupported attributes arrive as non-number values
        // and safely fall through to the optional SPI/title/bounds matcher.
        let window_number = CFString::from_static_str("AXWindowNumber");
        let role = CFString::from_static_str("AXRole");
        let subrole = CFString::from_static_str("AXSubrole");
        let window_attributes = CFArray::from_objects(&[
            &*title,
            &*minimized,
            &*position,
            &*size,
            &*window_number,
            &*role,
            &*subrole,
        ]);
        Self {
            windows,
            focused_window,
            title,
            window_attributes,
        }
    }
}

pub fn discover_windows(
    apps: &[AppSeed],
    displays: &[DisplayGeometry],
    should_cancel: impl Fn() -> bool,
) -> Option<DiscoveryResult> {
    discover_windows_bounded(apps, displays, &should_cancel)
}

fn discover_windows_bounded(
    apps: &[AppSeed],
    displays: &[DisplayGeometry],
    should_cancel: &impl Fn() -> bool,
) -> Option<DiscoveryResult> {
    let deadline = Instant::now() + DISCOVERY_BUDGET;
    let cg_entries = copy_cg_entries();
    if should_cancel() {
        return None;
    }
    let z_order = cg_entries
        .iter()
        .map(|entry| WindowOrder {
            window_id: entry.entry.window_id,
            z_index: entry.entry.z_index,
        })
        .collect::<Vec<_>>();
    let names = AxNames::new();
    let mut windows =
        Vec::with_capacity(cg_entries.len().max(apps.len()).min(MAX_DISCOVERED_WINDOWS));

    // Query apps in current CG z-order. NSWorkspace's running-app order is
    // otherwise arbitrary, so one hostile AX client could consume the entire
    // deadline and permanently starve every app that happened to follow it.
    for app_index in discovery_app_order(apps, &cg_entries) {
        let app = &apps[app_index];
        if should_cancel() {
            return None;
        }
        if windows.len() == MAX_DISCOVERED_WINDOWS {
            break;
        }
        let entries = cg_entries
            .iter()
            .filter(|entry| entry.pid == app.pid)
            .collect::<Vec<_>>();
        let mut app_windows = if Instant::now() >= deadline {
            // This snapshot is already resident and requires no cross-process
            // messaging. Keep later apps as bounded synthetic cards rather
            // than dropping them when an earlier AX endpoint hangs.
            synthetic_windows_from_cg(app, &entries, MAX_DISCOVERED_WINDOWS - windows.len())
        } else {
            windows_for_app(app, &entries, displays, &names, deadline, should_cancel)?
        };
        app_windows.truncate(MAX_DISCOVERED_WINDOWS - windows.len());
        windows.extend(app_windows);
    }

    windows.sort_by_key(|window| {
        window
            .metadata
            .key
            .window_id
            .and_then(|id| {
                z_order
                    .iter()
                    .find(|entry| entry.window_id == id)
                    .map(|entry| entry.z_index)
            })
            .unwrap_or(usize::MAX)
    });

    Some(DiscoveryResult { windows, z_order })
}

fn discovery_app_order(apps: &[AppSeed], cg_entries: &[ParsedCgEntry]) -> Vec<usize> {
    let mut order = (0..apps.len()).collect::<Vec<_>>();
    order.sort_unstable_by_key(|&index| {
        let first_z = cg_entries
            .iter()
            .filter(|entry| entry.pid == apps[index].pid)
            .map(|entry| entry.entry.z_index)
            .min()
            .unwrap_or(usize::MAX);
        // Preserve the caller's frontmost-first ordering for apps without a
        // CG entry, such as apps whose windows are all minimized.
        (first_z, index)
    });
    order
}

fn synthetic_windows_from_cg(
    app: &AppSeed,
    entries: &[&ParsedCgEntry],
    limit: usize,
) -> Vec<ManagedWindow> {
    if limit == 0 {
        return Vec::new();
    }
    let ax_app = AxHandle(unsafe { AXUIElement::new_application(app.pid) });
    let mut windows = entries
        .iter()
        .filter(|parsed| !parsed.entry.title.is_empty() || parsed.entry.bounds.is_usable())
        .take(limit)
        .enumerate()
        .map(|(ordinal, parsed)| ManagedWindow {
            metadata: WindowMetadata {
                key: WindowKey {
                    pid: app.pid,
                    window_id: Some(parsed.entry.window_id),
                    fallback_ordinal: u16::try_from(ordinal).unwrap_or(u16::MAX),
                },
                title: parsed.entry.title.clone(),
                app_name: app.name.clone(),
                bundle_identifier: app.bundle_identifier.clone(),
                bounds: parsed.entry.bounds,
                minimized: false,
                synthetic_app: true,
            },
            ax_element: ax_app.clone(),
        })
        .collect::<Vec<_>>();
    if windows.is_empty() {
        windows.push(ManagedWindow {
            metadata: WindowMetadata {
                key: WindowKey {
                    pid: app.pid,
                    window_id: None,
                    fallback_ordinal: 0,
                },
                title: Box::default(),
                app_name: app.name.clone(),
                bundle_identifier: app.bundle_identifier.clone(),
                bounds: Bounds::default(),
                minimized: false,
                synthetic_app: true,
            },
            ax_element: ax_app,
        });
    }
    windows
}

#[derive(Clone, Debug)]
struct ParsedCgEntry {
    pid: ProcessId,
    entry: CgEntry,
}

/// Copies only stable process/window pairs from WindowServer. Unlike full
/// discovery this performs no AX messaging and does not request protected
/// names or images, making it suitable for the hot quick-switch path.
#[must_use]
pub fn live_window_snapshot() -> Option<LiveWindowSnapshot> {
    let raw_list =
        CGWindowListCopyWindowInfo(CGWindowListOption::ExcludeDesktopElements, 0 as CGWindowID)?;
    // CGWindowListCopyWindowInfo documents an array of CFDictionary values.
    let list: CFRetained<CFArray<CFType>> = unsafe { CFRetained::cast_unchecked(raw_list) };
    let mut identities = Vec::with_capacity(list.len().min(MAX_CG_ENTRIES));
    for item in list.iter() {
        let Ok(dictionary) = item.downcast::<CFDictionary>() else {
            continue;
        };
        let dictionary: CFRetained<CFDictionary<CFString, CFType>> =
            unsafe { CFRetained::cast_unchecked(dictionary) };
        if dictionary_number(&dictionary, unsafe { kCGWindowLayer }) != Some(0) {
            continue;
        }
        let Some(pid) = dictionary_number(&dictionary, unsafe { kCGWindowOwnerPID })
            .and_then(|value| ProcessId::try_from(value).ok())
        else {
            continue;
        };
        let Some(window_id) = dictionary_number(&dictionary, unsafe { kCGWindowNumber })
            .and_then(|value| WindowId::try_from(value).ok())
            .filter(|value| *value != 0)
        else {
            continue;
        };
        // Window numbers are unique within a single WindowServer snapshot.
        identities.push(WindowIdentity::new(pid, window_id));
        if identities.len() == MAX_CG_ENTRIES {
            break;
        }
    }
    Some(LiveWindowSnapshot { identities })
}

/// Focuses a stable key without performing full AX discovery.
///
/// Validation and focus are intentionally one operation: the target must be
/// present in a snapshot copied immediately before the private exact-window
/// request. Every non-exact condition fails closed so the caller can fall back
/// to the normal switcher instead of activating an arbitrary window from the
/// same application.
#[must_use]
pub fn quick_switch_window_exact(key: &WindowKey) -> ExactQuickSwitchResult {
    let Some(identity) = key.identity() else {
        return ExactQuickSwitchResult::InvalidKey;
    };
    quick_switch_identity_exact(identity)
}

/// Identity-level companion for callers that already use
/// [`crate::core::session::RecentWindowHistory`].
#[must_use]
pub fn quick_switch_identity_exact(identity: WindowIdentity) -> ExactQuickSwitchResult {
    match fresh_window_identity_is_live(identity) {
        None => return ExactQuickSwitchResult::SnapshotUnavailable,
        Some(false) => return ExactQuickSwitchResult::WindowClosed,
        Some(true) => {}
    }
    if !focus_private_window_exact(identity.pid, identity.window_id) {
        return ExactQuickSwitchResult::RequiresFullFocus;
    }
    if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(identity.pid) {
        let _ = app.unhide();
    }
    ExactQuickSwitchResult::Focused
}

fn fresh_window_identity_is_live(identity: WindowIdentity) -> Option<bool> {
    let raw_list =
        CGWindowListCopyWindowInfo(CGWindowListOption::ExcludeDesktopElements, 0 as CGWindowID)?;
    let list: CFRetained<CFArray<CFType>> = unsafe { CFRetained::cast_unchecked(raw_list) };
    for item in list.iter().take(MAX_CG_ENTRIES) {
        let Ok(dictionary) = item.downcast::<CFDictionary>() else {
            continue;
        };
        let dictionary: CFRetained<CFDictionary<CFString, CFType>> =
            unsafe { CFRetained::cast_unchecked(dictionary) };
        if dictionary_number(&dictionary, unsafe { kCGWindowLayer }) == Some(0)
            && dictionary_number(&dictionary, unsafe { kCGWindowOwnerPID })
                == Some(i64::from(identity.pid))
            && dictionary_number(&dictionary, unsafe { kCGWindowNumber })
                == Some(i64::from(identity.window_id))
        {
            return Some(true);
        }
    }
    Some(false)
}

fn copy_cg_entries() -> Vec<ParsedCgEntry> {
    let Some(raw_list) =
        CGWindowListCopyWindowInfo(CGWindowListOption::ExcludeDesktopElements, 0 as CGWindowID)
    else {
        return Vec::new();
    };
    // CGWindowListCopyWindowInfo documents an array of CFDictionary values.
    let list: CFRetained<CFArray<CFType>> = unsafe { CFRetained::cast_unchecked(raw_list) };
    let mut entries = Vec::with_capacity(list.len().min(MAX_CG_ENTRIES));

    for (z_index, item) in list.iter().enumerate() {
        let Ok(dictionary) = item.downcast::<CFDictionary>() else {
            continue;
        };
        // System dictionaries use CFString keys and heterogeneous CFType values.
        let dictionary: CFRetained<CFDictionary<CFString, CFType>> =
            unsafe { CFRetained::cast_unchecked(dictionary) };
        let Some(pid) = dictionary_number(&dictionary, unsafe { kCGWindowOwnerPID })
            .and_then(|value| i32::try_from(value).ok())
        else {
            continue;
        };
        let Some(window_id) = dictionary_number(&dictionary, unsafe { kCGWindowNumber })
            .and_then(|value| u32::try_from(value).ok())
        else {
            continue;
        };
        if dictionary_number(&dictionary, unsafe { kCGWindowLayer }) != Some(0) {
            continue;
        }
        let bounds = dictionary_rect(&dictionary, unsafe { kCGWindowBounds }).unwrap_or_default();
        let title = dictionary_string(&dictionary, unsafe { kCGWindowName })
            .unwrap_or_default()
            .into_boxed_str();
        let onscreen = dictionary_bool(&dictionary, unsafe { kCGWindowIsOnscreen });
        entries.push(ParsedCgEntry {
            pid,
            entry: CgEntry {
                window_id,
                bounds,
                title,
                onscreen,
                z_index,
            },
        });
        if entries.len() == MAX_CG_ENTRIES {
            break;
        }
    }
    entries
}

/// Resolves the focused window for one known frontmost process without
/// allocating a z-order vector or discovering any other application's AX tree.
/// The focused AX ID is validated against the same fresh CG snapshot; when AX
/// cannot expose an ID, the first explicitly on-screen layer-zero window is the
/// bounded fallback.
#[must_use]
pub fn live_focused_window_identity(pid: ProcessId) -> LiveFocusIdentity {
    let focused_ax_window = focused_ax_window_id(pid);
    let Some(raw_list) =
        CGWindowListCopyWindowInfo(CGWindowListOption::ExcludeDesktopElements, 0 as CGWindowID)
    else {
        return LiveFocusIdentity::SnapshotUnavailable;
    };
    let list: CFRetained<CFArray<CFType>> = unsafe { CFRetained::cast_unchecked(raw_list) };
    let mut first_onscreen_window = None;
    for item in list.iter().take(MAX_CG_ENTRIES) {
        let Ok(dictionary) = item.downcast::<CFDictionary>() else {
            continue;
        };
        let dictionary: CFRetained<CFDictionary<CFString, CFType>> =
            unsafe { CFRetained::cast_unchecked(dictionary) };
        if dictionary_number(&dictionary, unsafe { kCGWindowLayer }) != Some(0)
            || dictionary_number(&dictionary, unsafe { kCGWindowOwnerPID }) != Some(i64::from(pid))
        {
            continue;
        }
        let Some(window_id) = dictionary_number(&dictionary, unsafe { kCGWindowNumber })
            .and_then(|value| WindowId::try_from(value).ok())
            .filter(|value| *value != 0)
        else {
            continue;
        };
        if focused_ax_window == Some(window_id) {
            return LiveFocusIdentity::Focused(WindowIdentity::new(pid, window_id));
        }
        if first_onscreen_window.is_none()
            && dictionary_bool(&dictionary, unsafe { kCGWindowIsOnscreen }) == Some(true)
        {
            first_onscreen_window = Some(window_id);
        }
    }
    first_onscreen_window.map_or(LiveFocusIdentity::NoWindow, |window_id| {
        LiveFocusIdentity::Focused(WindowIdentity::new(pid, window_id))
    })
}

fn focused_ax_window_id(pid: ProcessId) -> Option<WindowId> {
    let app = unsafe { AXUIElement::new_application(pid) };
    let _ = unsafe { app.set_messaging_timeout(FOCUS_AX_TIMEOUT_SECONDS) };
    let focused_name = CFString::from_static_str("AXFocusedWindow");
    let focused = ax_element_attribute(&app, &focused_name)?
        .downcast::<AXUIElement>()
        .ok()?;
    let _ = unsafe { focused.set_messaging_timeout(FOCUS_AX_TIMEOUT_SECONDS) };

    // The dynamically resolved SPI is a local identity lookup and avoids a
    // second cross-process message in the direct build. The App Store build
    // compiles it out and uses the optional AXWindowNumber attribute instead.
    private_ax_window_id(&focused).or_else(|| {
        let window_number = CFString::from_static_str("AXWindowNumber");
        cf_window_id_value(ax_element_attribute(&focused, &window_number))
    })
}

fn windows_for_app(
    app: &AppSeed,
    parsed_entries: &[&ParsedCgEntry],
    displays: &[DisplayGeometry],
    names: &AxNames,
    deadline: Instant,
    should_cancel: &impl Fn() -> bool,
) -> Option<Vec<ManagedWindow>> {
    let entries = parsed_entries
        .iter()
        .map(|entry| entry.entry.clone())
        .collect::<Vec<_>>();
    let ax_app = unsafe { AXUIElement::new_application(app.pid) };
    let _ = unsafe { ax_app.set_messaging_timeout(DISCOVERY_AX_TIMEOUT_SECONDS) };
    let ax_app = AxHandle(ax_app);
    let mut used = vec![false; entries.len()];
    let ax_windows = ax_window_list(&ax_app.0, &names.windows).unwrap_or_else(|| {
        ax_element_attribute(&ax_app.0, &names.focused_window)
            .and_then(|value| value.downcast::<AXUIElement>().ok())
            .map(|window| vec![window])
            .unwrap_or_default()
    });
    let mut windows = Vec::with_capacity(ax_windows.len().min(MAX_AX_WINDOWS_PER_APP));
    let mut consecutive_failures = 0;
    for (ordinal, ax_window) in ax_windows.into_iter().enumerate() {
        if should_cancel() {
            return None;
        }
        if Instant::now() >= deadline || consecutive_failures == MAX_CONSECUTIVE_AX_FAILURES {
            break;
        }
        if let Some(window) = parse_ax_window(
            app,
            AxHandle(ax_window),
            ordinal,
            &entries,
            &mut used,
            displays,
            names,
        ) {
            windows.push(window);
            consecutive_failures = 0;
        } else {
            consecutive_failures += 1;
        }
    }

    let ax_found_windows = !windows.is_empty();
    for (index, entry) in entries.iter().enumerate() {
        if should_cancel() {
            return None;
        }
        if used[index] {
            continue;
        }
        if ax_found_windows {
            if !should_include_unmatched_cg_entry(
                entry,
                displays,
                app.bundle_identifier.as_deref() == Some("com.apple.finder"),
            ) {
                continue;
            }
            if can_suppress_as_current_space_duplicate(entry.onscreen)
                && windows.iter().any(|window| {
                    !window.metadata.synthetic_app
                        && bounds_near(window.metadata.bounds, entry.bounds, displays, 20.0)
                })
            {
                continue;
            }
        } else if entry.title.is_empty() && !entry.bounds.is_usable() {
            continue;
        }

        used[index] = true;
        windows.push(ManagedWindow {
            metadata: WindowMetadata {
                key: WindowKey {
                    pid: app.pid,
                    window_id: Some(entry.window_id),
                    fallback_ordinal: u16::try_from(index).unwrap_or(u16::MAX),
                },
                title: entry.title.clone(),
                app_name: app.name.clone(),
                bundle_identifier: app.bundle_identifier.clone(),
                bounds: entry.bounds,
                minimized: false,
                synthetic_app: true,
            },
            ax_element: ax_app.clone(),
        });
    }

    if windows.is_empty() {
        windows.push(ManagedWindow {
            metadata: WindowMetadata {
                key: WindowKey {
                    pid: app.pid,
                    window_id: None,
                    fallback_ordinal: 0,
                },
                title: Box::default(),
                app_name: app.name.clone(),
                bundle_identifier: app.bundle_identifier.clone(),
                bounds: Bounds::default(),
                minimized: false,
                synthetic_app: true,
            },
            ax_element: ax_app,
        });
    }

    Some(deduplicate_windows(windows))
}

fn can_suppress_as_current_space_duplicate(onscreen: Option<bool>) -> bool {
    // Only an explicit true is enough to merge by geometry. Both false and an
    // absent optional key can represent a distinct window on another Space.
    onscreen == Some(true)
}

fn should_include_unmatched_cg_entry(
    entry: &CgEntry,
    displays: &[DisplayGeometry],
    retain_untitled_usable: bool,
) -> bool {
    if !entry.bounds.is_usable() {
        return false;
    }

    // A named, usable layer-zero window is strong enough evidence to retain
    // even when it is not almost fullscreen. This covers maximized/zoomed
    // windows on ultrawide displays, windows constrained to the visible frame,
    // and ordinary windows on another Space that AX omitted from AXWindows.
    let intersects_display = displays.is_empty()
        || displays
            .iter()
            .any(|display| bounds_overlap(entry.bounds, display.frame));
    if !intersects_display {
        return false;
    }
    if !entry.title.trim().is_empty() {
        return true;
    }

    // Finder's real layer-zero windows can lose kCGWindowName when Screen
    // Recording is unavailable. ExcludeDesktopElements already removed the
    // Desktop/background surfaces, so their stable PID/window IDs are better
    // evidence than an optional title. This preserves ordinary off-Space
    // Finder windows, not just maximized/fullscreen ones.
    if retain_untitled_usable {
        return true;
    }

    // kCGWindowName is optional even with Screen Recording permission. Keep an
    // untitled gap when its geometry is display-sized; requiring a title here
    // made legitimate fullscreen windows disappear. Small untitled surfaces
    // remain excluded to avoid Electron/GPU helper cards.
    displays.is_empty()
        || displays.iter().any(|display| {
            entry.bounds.is_fullscreen_like(display.frame)
                && bounds_overlap(entry.bounds, display.frame)
        })
}

#[allow(clippy::too_many_arguments)]
fn parse_ax_window(
    app: &AppSeed,
    ax_element: AxHandle,
    ordinal: usize,
    entries: &[CgEntry],
    used: &mut [bool],
    displays: &[DisplayGeometry],
    names: &AxNames,
) -> Option<ManagedWindow> {
    // AX messaging timeouts are element-local. Setting one only on the parent
    // application does not bound calls made through this child window.
    let _ = unsafe {
        ax_element
            .0
            .set_messaging_timeout(DISCOVERY_AX_TIMEOUT_SECONDS)
    };
    let values = ax_multiple_attributes(&ax_element.0, &names.window_attributes)?;
    let title = cf_string_value(values.get(0)).unwrap_or_default();
    let minimized = cf_bool_value(values.get(1)).unwrap_or(false);
    let position = cf_point_value(values.get(2)).unwrap_or_default();
    let size = cf_size_value(values.get(3)).unwrap_or_default();
    let bounds = Bounds::new(position.x, position.y, size.width, size.height);
    if !minimized && title.is_empty() && !bounds.is_usable() {
        return None;
    }
    let exact_window_id =
        cf_window_id_value(values.get(4)).or_else(|| private_ax_window_id(&ax_element.0));
    let matched = best_unused_match(exact_window_id, &title, bounds, entries, used, displays);
    let role = cf_string_value(values.get(5));
    let subrole = cf_string_value(values.get(6));
    if app.bundle_identifier.as_deref() == Some("com.apple.finder")
        && !is_focusable_finder_ax_window(matched.is_some(), role.as_deref(), subrole.as_deref())
    {
        return None;
    }
    if let Some(index) = matched {
        used[index] = true;
    }

    Some(ManagedWindow {
        metadata: WindowMetadata {
            key: WindowKey {
                pid: app.pid,
                window_id: matched.map(|index| entries[index].window_id),
                fallback_ordinal: u16::try_from(ordinal).unwrap_or(u16::MAX),
            },
            title: title.into_boxed_str(),
            app_name: app.name.clone(),
            bundle_identifier: app.bundle_identifier.clone(),
            bounds,
            minimized,
            synthetic_app: false,
        },
        ax_element,
    })
}

fn is_focusable_finder_ax_window(
    has_stable_window_id: bool,
    role: Option<&str>,
    subrole: Option<&str>,
) -> bool {
    let is_browser_or_dialog =
        role == Some("AXWindow") && matches!(subrole, Some("AXStandardWindow" | "AXDialog"));
    is_browser_or_dialog || (has_stable_window_id && role.is_none() && subrole.is_none())
}

fn best_unused_match(
    exact_window_id: Option<WindowId>,
    ax_title: &str,
    ax_bounds: Bounds,
    entries: &[CgEntry],
    used: &[bool],
    displays: &[DisplayGeometry],
) -> Option<usize> {
    if let Some(exact_window_id) = exact_window_id
        && let Some(index) = entries
            .iter()
            .enumerate()
            .position(|(index, entry)| !used[index] && entry.window_id == exact_window_id)
    {
        return Some(index);
    }

    // Multiple windows from Arc, Chromium, Electron, and fullscreen apps can
    // occupy exactly the same desktop bounds on different Spaces. A greedy
    // geometry-only join silently swaps their thumbnails and CG identities.
    // Rank a non-empty title agreement ahead of geometry, then refuse a true
    // tie rather than attaching a confidently wrong thumbnail to an AX card.
    let mut best: Option<(usize, u8, f64)> = None;
    let mut runner_up: Option<(usize, u8, f64)> = None;
    for (index, entry) in entries.iter().enumerate() {
        if used[index] {
            continue;
        }
        let distance = match_score(ax_bounds, entry.bounds, displays);
        if distance >= MATCH_DISTANCE_LIMIT {
            continue;
        }
        let candidate = (index, title_match_rank(ax_title, &entry.title), distance);
        if best
            .as_ref()
            .is_none_or(|current| match_candidate_order(&candidate, current).is_lt())
        {
            runner_up = best.replace(candidate);
        } else if runner_up
            .as_ref()
            .is_none_or(|current| match_candidate_order(&candidate, current).is_lt())
        {
            runner_up = Some(candidate);
        }
    }
    let best = best?;
    if let Some(second) = runner_up
        && second.1 == best.1
        && (second.2 - best.2).abs() <= MATCH_AMBIGUITY_EPSILON
    {
        return None;
    }
    Some(best.0)
}

fn match_candidate_order(left: &(usize, u8, f64), right: &(usize, u8, f64)) -> std::cmp::Ordering {
    left.1
        .cmp(&right.1)
        .then_with(|| left.2.total_cmp(&right.2))
}

fn title_match_rank(ax_title: &str, cg_title: &str) -> u8 {
    let ax_title = ax_title.trim();
    let cg_title = cg_title.trim();
    if ax_title.is_empty() || cg_title.is_empty() {
        return 2;
    }
    if ax_title == cg_title || ax_title.eq_ignore_ascii_case(cg_title) {
        return 0;
    }
    let shorter_len = ax_title.chars().count().min(cg_title.chars().count());
    if shorter_len >= 8
        && (ax_title.contains(cg_title)
            || cg_title.contains(ax_title)
            || ascii_case_insensitive_contains(ax_title, cg_title)
            || ascii_case_insensitive_contains(cg_title, ax_title))
    {
        1
    } else {
        2
    }
}

fn ascii_case_insensitive_contains(haystack: &str, needle: &str) -> bool {
    haystack.is_ascii()
        && needle.is_ascii()
        && haystack
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn match_score(ax: Bounds, cg: Bounds, _displays: &[DisplayGeometry]) -> f64 {
    // AX window positions and CGWindowList bounds both use the global desktop
    // top-left coordinate space. Applying an NSScreen-style vertical flip here
    // can make two unrelated windows appear to occupy exactly the same place.
    let origin_distance = (ax.x - cg.x).hypot(ax.y - cg.y);
    let size_penalty = ((ax.width - cg.width).abs() + (ax.height - cg.height).abs()) * 0.1;
    origin_distance + size_penalty
}

fn bounds_near(ax: Bounds, cg: Bounds, displays: &[DisplayGeometry], tolerance: f64) -> bool {
    match_score(ax, cg, displays) < tolerance
        && (ax.width - cg.width).abs() < tolerance
        && (ax.height - cg.height).abs() < tolerance
}

fn bounds_overlap(left: Bounds, right: Bounds) -> bool {
    left.x < right.x + right.width
        && left.x + left.width > right.x
        && left.y < right.y + right.height
        && left.y + left.height > right.y
}

fn deduplicate_windows(windows: Vec<ManagedWindow>) -> Vec<ManagedWindow> {
    let mut deduped: Vec<ManagedWindow> = Vec::with_capacity(windows.len());
    for window in windows {
        let Some(id) = window.metadata.key.window_id else {
            deduped.push(window);
            continue;
        };
        if let Some(existing) = deduped
            .iter_mut()
            .find(|candidate| candidate.metadata.key.window_id == Some(id))
        {
            if !window.metadata.synthetic_app && existing.metadata.synthetic_app {
                *existing = window;
            }
        } else {
            deduped.push(window);
        }
    }
    deduped
}

fn dictionary_number(dictionary: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<i64> {
    dictionary.get(key)?.downcast::<CFNumber>().ok()?.as_i64()
}

fn dictionary_string(
    dictionary: &CFDictionary<CFString, CFType>,
    key: &CFString,
) -> Option<String> {
    dictionary
        .get(key)?
        .downcast::<CFString>()
        .ok()
        .map(|value| value.to_string())
}

fn dictionary_bool(dictionary: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<bool> {
    dictionary
        .get(key)?
        .downcast::<CFBoolean>()
        .ok()
        .map(|value| value.as_bool())
}

fn dictionary_rect(dictionary: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<Bounds> {
    let raw = dictionary.get(key)?.downcast::<CFDictionary>().ok()?;
    let mut rect = CGRect::default();
    if !unsafe { CGRectMakeWithDictionaryRepresentation(Some(&raw), &mut rect) } {
        return None;
    }
    Some(Bounds::new(
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    ))
}

fn ax_element_attribute(element: &AXUIElement, name: &CFString) -> Option<CFRetained<CFType>> {
    let mut raw: *const CFType = ptr::null();
    let destination = NonNull::from(&mut raw);
    let result = unsafe { element.copy_attribute_value(name, destination) };
    if result != AXError::Success {
        return None;
    }
    let raw = NonNull::new(raw.cast_mut())?;
    Some(unsafe { CFRetained::from_raw(raw) })
}

fn ax_multiple_attributes(
    element: &AXUIElement,
    attributes: &CFArray<CFString>,
) -> Option<CFRetained<CFArray<CFType>>> {
    let mut raw: *const CFArray = ptr::null();
    let destination = NonNull::from(&mut raw);
    let result = unsafe {
        element.copy_multiple_attribute_values(
            attributes.as_opaque(),
            AXCopyMultipleAttributeOptions::empty(),
            destination,
        )
    };
    if result != AXError::Success {
        return None;
    }
    let raw = NonNull::new(raw.cast_mut())?;
    let values = unsafe { CFRetained::<CFArray>::from_raw(raw) };
    Some(unsafe { CFRetained::cast_unchecked(values) })
}

fn ax_window_list(element: &AXUIElement, name: &CFString) -> Option<Vec<CFRetained<AXUIElement>>> {
    let array = ax_element_attribute(element, name)?
        .downcast::<CFArray>()
        .ok()?;
    let array: CFRetained<CFArray<CFType>> = unsafe { CFRetained::cast_unchecked(array) };
    let windows = array
        .iter()
        .filter_map(|item| item.downcast::<AXUIElement>().ok())
        .take(MAX_AX_WINDOWS_PER_APP)
        .collect::<Vec<_>>();
    (!windows.is_empty()).then_some(windows)
}

fn ax_string_attribute(element: &AXUIElement, name: &CFString) -> Option<String> {
    ax_element_attribute(element, name)?
        .downcast::<CFString>()
        .ok()
        .map(|value| value.to_string())
}

fn cf_string_value(value: Option<CFRetained<CFType>>) -> Option<String> {
    value?
        .downcast::<CFString>()
        .ok()
        .map(|value| value.to_string())
}

fn cf_bool_value(value: Option<CFRetained<CFType>>) -> Option<bool> {
    value?
        .downcast::<CFBoolean>()
        .ok()
        .map(|value| value.as_bool())
}

fn cf_window_id_value(value: Option<CFRetained<CFType>>) -> Option<WindowId> {
    value?
        .downcast::<CFNumber>()
        .ok()?
        .as_i64()
        .and_then(|value| WindowId::try_from(value).ok())
        .filter(|value| *value != 0)
}

#[cfg(not(feature = "app-store"))]
type AxUiElementGetWindow = unsafe extern "C" fn(*const AXUIElement, *mut WindowId) -> AXError;

#[cfg(not(feature = "app-store"))]
fn private_ax_window_id(element: &AXUIElement) -> Option<WindowId> {
    // This SPI is already present in the linked ApplicationServices image on
    // supported macOS versions. Resolve it lazily so symbol absence remains a
    // harmless fallback and the App Store feature can compile it out entirely.
    static GET_WINDOW: OnceLock<Option<AxUiElementGetWindow>> = OnceLock::new();
    let get_window = *GET_WINDOW.get_or_init(|| {
        let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"_AXUIElementGetWindow".as_ptr()) };
        (!symbol.is_null())
            .then(|| unsafe { std::mem::transmute::<*mut c_void, AxUiElementGetWindow>(symbol) })
    });
    let get_window = get_window?;
    let mut window_id = 0;
    let result = unsafe { get_window(ptr::from_ref(element), &mut window_id) };
    (result == AXError::Success && window_id != 0).then_some(window_id)
}

#[cfg(feature = "app-store")]
fn private_ax_window_id(_element: &AXUIElement) -> Option<WindowId> {
    None
}

fn cf_point_value(value: Option<CFRetained<CFType>>) -> Option<CGPoint> {
    let value = value?.downcast::<AXValue>().ok()?;
    let mut point = CGPoint::default();
    let destination = NonNull::from(&mut point).cast();
    unsafe { value.value(AXValueType::CGPoint, destination) }.then_some(point)
}

fn cf_size_value(value: Option<CFRetained<CFType>>) -> Option<CGSize> {
    let value = value?.downcast::<AXValue>().ok()?;
    let mut size = CGSize::default();
    let destination = NonNull::from(&mut size).cast();
    unsafe { value.value(AXValueType::CGSize, destination) }.then_some(size)
}

/// Best-effort focus of `window_id`, including its owning Space.
///
/// SkyLight is private SPI, so this path is deliberately optional: load and
/// symbol failures, a null result, an unexpected CF type, or an empty result
/// all become a no-op. `focus_window` always continues through the public AX
/// and NSRunningApplication activation path afterward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrivateFocusResult {
    Unavailable,
    ExactWindow,
    #[cfg(not(feature = "app-store"))]
    LegacySpace,
}

#[cfg(feature = "app-store")]
fn switch_to_window_space(_pid: ProcessId, _window_id: WindowId) -> PrivateFocusResult {
    PrivateFocusResult::Unavailable
}

#[cfg(feature = "app-store")]
fn focus_private_window_exact(_pid: ProcessId, _window_id: WindowId) -> bool {
    false
}

#[cfg(not(feature = "app-store"))]
fn private_focus_is_allowed() -> bool {
    if std::env::var_os("MACFLOW_DISABLE_PRIVATE_SPACES").is_some() {
        return false;
    }
    let major = NSProcessInfo::processInfo()
        .operatingSystemVersion()
        .majorVersion;
    (14..=26).contains(&major)
}

#[cfg(not(feature = "app-store"))]
fn set_front_window_exact(api: &SkyLightApi, pid: ProcessId, window_id: WindowId) -> bool {
    let (Some(get_process_for_pid), Some(set_front_process)) =
        (api.get_process_for_pid, api.set_front_process)
    else {
        return false;
    };
    let mut psn = ProcessSerialNumber { high: 0, low: 0 };
    let psn_status = unsafe { get_process_for_pid(pid, &mut psn) };
    if psn_status != 0 {
        return false;
    }
    // 0x200 is SkyLight's user-generated flag. It prevents the focus request
    // from being suppressed as background automation.
    let front_status = unsafe { set_front_process(&mut psn, window_id, 0x200) };
    if front_status != 0 {
        return false;
    }
    if let Some(post_event_record) = api.post_event_record {
        make_key_window(post_event_record, &mut psn, window_id);
    }
    true
}

#[cfg(not(feature = "app-store"))]
fn focus_private_window_exact(pid: ProcessId, window_id: WindowId) -> bool {
    if !private_focus_is_allowed() || !set_front_window_exact(skylight(), pid, window_id) {
        return false;
    }
    notify_spaces_changed();
    true
}

#[cfg(not(feature = "app-store"))]
fn switch_to_window_space(pid: ProcessId, window_id: WindowId) -> PrivateFocusResult {
    // SkyLight is private ABI. Keep a remote kill switch and call it only on
    // macOS majors whose signatures have been smoke-tested for this release.
    // Outside that envelope the public AX/app-activation fallback still runs.
    if !private_focus_is_allowed() {
        return PrivateFocusResult::Unavailable;
    }

    let api = skylight();

    // Modern macOS treats NSRunningApplication activation as advisory. This
    // WindowServer call fronts one exact process/window pair and naturally
    // enters the Space displaying it. It is the primary path used by current
    // production switchers; the older explicit ShowSpaces path below remains
    // a fail-closed fallback for a missing symbol or rejected request.
    if set_front_window_exact(api, pid, window_id) {
        notify_spaces_changed();
        return PrivateFocusResult::ExactWindow;
    }

    let (Some(default_connection), Some(copy_spaces), Some(show_spaces)) = (
        api.default_connection,
        api.copy_spaces_for_windows,
        api.show_spaces,
    ) else {
        return PrivateFocusResult::Unavailable;
    };

    let window_number = CFNumber::new_i64(i64::from(window_id));
    let window_numbers = CFArray::from_objects(&[&*window_number]);
    // SAFETY: `default_connection` is the resolved zero-argument C function.
    let connection = unsafe { default_connection() };
    if connection == 0 {
        return PrivateFocusResult::Unavailable;
    }
    // SAFETY: The functions were resolved from the matching SkyLight image.
    // `window_numbers` is a valid CFArray for the duration of both calls.
    let raw_spaces = unsafe {
        copy_spaces(
            connection,
            -1_isize,
            CFRetained::as_ptr(&window_numbers).as_ptr().cast(),
        )
    };
    let Some(raw_spaces) = NonNull::new(raw_spaces.cast_mut()) else {
        return PrivateFocusResult::Unavailable;
    };
    // SAFETY: A `Copy`-family CF function returns a +1 object. It is first
    // represented as CFType so the runtime type can be validated before use.
    let raw_spaces = unsafe { CFRetained::<CFType>::from_raw(raw_spaces) };
    let Ok(spaces) = raw_spaces.downcast::<CFArray>() else {
        return PrivateFocusResult::Unavailable;
    };
    // `CFArray`'s public default generic is opaque; the API contract says its
    // contents are CF objects, so cast only after validating the outer type.
    let spaces: CFRetained<CFArray<CFType>> = unsafe { CFRetained::cast_unchecked(spaces) };
    if spaces.is_empty()
        || !spaces
            .iter()
            .all(|space| space.downcast::<CFNumber>().is_ok())
    {
        return PrivateFocusResult::Unavailable;
    }

    // SAFETY: The array remains retained across the call and has a verified
    // CFArray runtime type with at least one CFNumber Space identifier.
    unsafe { show_spaces(connection, CFRetained::as_ptr(&spaces).as_ptr().cast()) };
    // Keep the Dock/Mission Control model in sync with the private Space jump.
    // This mirrors the original implementation and prevents a later activation
    // from snapping back to the Space that macOS still considers current.
    notify_spaces_changed();
    PrivateFocusResult::LegacySpace
}

#[cfg(not(feature = "app-store"))]
fn notify_spaces_changed() {
    let name = NSString::from_str("com.apple.spaces.userchangedspaces");
    unsafe {
        NSNotificationCenter::defaultCenter().postNotificationName_object(&name, None);
    }
}

#[cfg(not(feature = "app-store"))]
fn make_key_window(
    post_event_record: SlpsPostEventRecordTo,
    psn: &mut ProcessSerialNumber,
    window_id: WindowId,
) {
    // Reverse-engineered CGSEventRecord layout used by WindowServer. The down
    // event makes the addressed window key. Always balance it with mouse-up so
    // the destination process cannot retain a pressed-pointer/menu-tracking
    // state after repeated same-app Space switches. Both events use an
    // off-content point, so the pair cannot activate a title-bar/content control.
    let (mut down, mut up) = make_key_event_records(window_id);
    // SAFETY: Both records are live, fixed 0x100-byte buffers matching the ABI
    // and remain valid until their synchronous calls return. Attempt mouse-up
    // even if the down call reports failure because it may still have delivered.
    let _ = unsafe { post_event_record(psn, down.as_mut_ptr()) };
    let _ = unsafe { post_event_record(psn, up.as_mut_ptr()) };
}

#[cfg(not(feature = "app-store"))]
fn make_key_event_records(window_id: WindowId) -> ([u8; 0x100], [u8; 0x100]) {
    (
        make_key_event_record(window_id, 0x01),
        make_key_event_record(window_id, 0x02),
    )
}

#[cfg(not(feature = "app-store"))]
fn make_key_event_record(window_id: WindowId, event_type: u8) -> [u8; 0x100] {
    const BUFFER_SIZE: usize = 0x100;
    const RECORD_LENGTH_OFFSET: usize = 0x04;
    const EVENT_TYPE_OFFSET: usize = 0x08;
    const WINDOW_LOCATION_OFFSET: usize = 0x20;
    const UNKNOWN_FLAG_OFFSET: usize = 0x3a;
    const WINDOW_ID_OFFSET: usize = 0x3c;

    let mut record = [0_u8; BUFFER_SIZE];
    record[RECORD_LENGTH_OFFSET] = 0xf8;
    record[EVENT_TYPE_OFFSET] = event_type;
    record[UNKNOWN_FLAG_OFFSET] = 0x10;
    record[WINDOW_ID_OFFSET..WINDOW_ID_OFFSET + size_of::<WindowId>()]
        .copy_from_slice(&window_id.to_ne_bytes());
    let off_content = [300_000_f64, 300_000_f64];
    // CGPoint is two native f64 values on every supported 64-bit macOS build.
    let point_bytes = unsafe {
        std::slice::from_raw_parts(off_content.as_ptr().cast::<u8>(), size_of_val(&off_content))
    };
    record[WINDOW_LOCATION_OFFSET..WINDOW_LOCATION_OFFSET + point_bytes.len()]
        .copy_from_slice(point_bytes);
    record
}

fn focus_ax_window(ax_app: &AXUIElement, ax_window: &AXUIElement) {
    let true_value: &CFType = CFBoolean::new(true).as_ref();
    let _ =
        unsafe { ax_window.set_attribute_value(&CFString::from_static_str("AXMain"), true_value) };
    let focused_value: &CFType = ax_window;
    let _ = unsafe {
        ax_app.set_attribute_value(&CFString::from_static_str("AXFocusedWindow"), focused_value)
    };
    let _ = unsafe { ax_window.perform_action(&CFString::from_static_str("AXRaise")) };
}

fn focus_repair_rank(
    target_id: Option<WindowId>,
    target_title: &str,
    candidate_id: Option<WindowId>,
    candidate_title: &str,
) -> Option<u8> {
    if let Some(target_id) = target_id {
        // A known CG window ID is the identity contract. Falling back to a
        // title when a candidate reports a different ID can focus the wrong
        // Finder folder, Arc tab, or editor when titles are duplicated.
        return (candidate_id == Some(target_id)).then_some(0);
    }
    (!target_title.is_empty() && candidate_title == target_title).then_some(1)
}

fn schedule_focus_repair(
    pid: ProcessId,
    window_id: Option<WindowId>,
    title: Box<str>,
    ax_window: Option<AxHandle>,
    allow_app_only_fallback: bool,
    sequence: u64,
    delay: Duration,
) {
    if window_id.is_none() && title.is_empty() && ax_window.is_none() {
        return;
    }
    let Ok(when) = DispatchTime::try_from(delay) else {
        return;
    };
    let _ = DispatchQueue::main().after(when, move || {
        // A newer selection supersedes this delayed repair, preventing a slow
        // AX response from pulling focus back to a previously chosen window.
        if FOCUS_SEQUENCE.load(Ordering::Acquire) != sequence {
            return;
        }
        let deadline = Instant::now() + SYNTHETIC_FOCUS_BUDGET;
        let ax_app = unsafe { AXUIElement::new_application(pid) };
        let _ = unsafe { ax_app.set_messaging_timeout(FOCUS_AX_TIMEOUT_SECONDS) };
        let frontmost = CFString::from_static_str("AXFrontmost");
        let true_value: &CFType = CFBoolean::new(true).as_ref();

        // Normal AX-backed windows already carry the exact retained child.
        // Reusing it avoids a title lookup and makes same-app Arc/VS Code
        // fullscreen switching deterministic after the Space animation.
        if let Some(ax_window) = ax_window {
            let _ = unsafe { ax_app.set_attribute_value(&frontmost, true_value) };
            let _ = unsafe { ax_window.0.set_messaging_timeout(FOCUS_AX_TIMEOUT_SECONDS) };
            focus_ax_window(&ax_app, &ax_window.0);
            return;
        }

        let names = AxNames::new();
        let window_number = CFString::from_static_str("AXWindowNumber");
        let mut title_match: Option<CFRetained<AXUIElement>> = None;
        let mut title_is_ambiguous = false;
        if let Some(windows) = ax_window_list(&ax_app, &names.windows) {
            for ax_window in windows.into_iter().take(MAX_SYNTHETIC_TITLE_SCAN) {
                if FOCUS_SEQUENCE.load(Ordering::Acquire) != sequence || Instant::now() >= deadline
                {
                    return;
                }
                let _ = unsafe { ax_window.set_messaging_timeout(FOCUS_AX_TIMEOUT_SECONDS) };
                let candidate_id = private_ax_window_id(&ax_window).or_else(|| {
                    cf_window_id_value(ax_element_attribute(&ax_window, &window_number))
                });
                let candidate_title = ax_string_attribute(&ax_window, &names.title);
                match focus_repair_rank(
                    window_id,
                    &title,
                    candidate_id,
                    candidate_title.as_deref().unwrap_or_default(),
                ) {
                    Some(0) => {
                        let _ = unsafe { ax_app.set_attribute_value(&frontmost, true_value) };
                        focus_ax_window(&ax_app, &ax_window);
                        return;
                    }
                    Some(1) => {
                        if title_match.is_some() {
                            title_is_ambiguous = true;
                        } else {
                            title_match = Some(ax_window);
                        }
                    }
                    _ => {}
                }
            }
            // Title is only a fallback for synthetic entries whose AX server
            // exposes no window number. Never guess between identical Arc tabs.
            if !title_is_ambiguous && let Some(ax_window) = title_match {
                let _ = unsafe { ax_app.set_attribute_value(&frontmost, true_value) };
                focus_ax_window(&ax_app, &ax_window);
                return;
            }
        }

        if FOCUS_SEQUENCE.load(Ordering::Acquire) != sequence || Instant::now() >= deadline {
            return;
        }
        if allow_app_only_fallback {
            let _ = unsafe { ax_app.set_attribute_value(&frontmost, true_value) };
        }
    });
}

/// Invalidates any delayed AX title repair queued for a prior switch action.
/// Call this when beginning or cancelling a switcher presentation so an old
/// synthetic-window callback cannot steal focus from the new interaction.
pub fn cancel_pending_focus() {
    let _ = next_focus_sequence();
}

fn next_focus_sequence() -> u64 {
    FOCUS_SEQUENCE
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1)
}

pub fn focus_window(window: &ManagedWindow) {
    let sequence = next_focus_sequence();
    let minimized = CFString::from_static_str("AXMinimized");
    let frontmost = CFString::from_static_str("AXFrontmost");
    let was_frontmost = NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .is_some_and(|app| app.processIdentifier() == window.metadata.key.pid);
    let _ = unsafe {
        window
            .ax_element
            .0
            .set_messaging_timeout(FOCUS_AX_TIMEOUT_SECONDS)
    };
    if window.metadata.minimized {
        let false_value: &CFType = CFBoolean::new(false).as_ref();
        let _ = unsafe {
            window
                .ax_element
                .0
                .set_attribute_value(&minimized, false_value)
        };
    }

    let ax_app = unsafe { AXUIElement::new_application(window.metadata.key.pid) };
    let _ = unsafe { ax_app.set_messaging_timeout(FOCUS_AX_TIMEOUT_SECONDS) };

    let private_focus = window
        .metadata
        .key
        .window_id
        .map(|window_id| switch_to_window_space(window.metadata.key.pid, window_id))
        .unwrap_or(PrivateFocusResult::Unavailable);

    if window.metadata.synthetic_app {
        // Exact WindowServer focus already selected the requested window. An
        // app-only AXFrontmost write here can immediately restore Finder's or
        // another multi-window app's previous key window, so use it only when
        // the exact path was unavailable.
        if private_focus != PrivateFocusResult::ExactWindow {
            let true_value: &CFType = CFBoolean::new(true).as_ref();
            let _ = unsafe { ax_app.set_attribute_value(&frontmost, true_value) };
        }
    } else {
        let true_value: &CFType = CFBoolean::new(true).as_ref();
        focus_ax_window(&ax_app, &window.ax_element.0);
        let _ = unsafe { ax_app.set_attribute_value(&frontmost, true_value) };
    }

    if let Some(app) =
        NSRunningApplication::runningApplicationWithProcessIdentifier(window.metadata.key.pid)
    {
        let _ = app.unhide();
        if !was_frontmost && private_focus != PrivateFocusResult::ExactWindow {
            // Activation is needed when crossing applications, but can restore
            // an already-frontmost app's old key window. Skip it for same-app
            // switching and reassert the exact AX child after cross-app
            // activation so VS Code and similar multi-window apps stay exact.
            let _ = app.activateWithOptions(NSApplicationActivationOptions::empty());
            if !window.metadata.synthetic_app {
                focus_ax_window(&ax_app, &window.ax_element.0);
            }
        }
    }

    // `SLSShowSpaces` starts an asynchronous desktop/fullscreen transition.
    // AX calls made above can therefore succeed against the old Space and then
    // be undone by WindowServer. Reassert the exact child during and after the
    // animation. The global sequence cancels both callbacks on any newer show,
    // cancel, or confirm, so an old repair can never steal focus later.
    if private_focus != PrivateFocusResult::Unavailable
        || !was_frontmost
        || window.metadata.synthetic_app
    {
        for delay in [FIRST_FOCUS_REPAIR_DELAY, FINAL_FOCUS_REPAIR_DELAY] {
            schedule_focus_repair(
                window.metadata.key.pid,
                window.metadata.key.window_id,
                window.metadata.title.clone(),
                (!window.metadata.synthetic_app).then(|| window.ax_element.clone()),
                private_focus != PrivateFocusResult::ExactWindow,
                sequence,
                delay,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn displays() -> Vec<DisplayGeometry> {
        vec![
            DisplayGeometry {
                frame: Bounds::new(0.0, 0.0, 1920.0, 1080.0),
                is_main: true,
            },
            DisplayGeometry {
                frame: Bounds::new(-1280.0, 80.0, 1280.0, 1024.0),
                is_main: false,
            },
        ]
    }

    fn app(pid: ProcessId, name: &str) -> AppSeed {
        AppSeed {
            pid,
            name: name.into(),
            bundle_identifier: None,
        }
    }

    fn parsed_entry(pid: ProcessId, window_id: WindowId, z_index: usize) -> ParsedCgEntry {
        ParsedCgEntry {
            pid,
            entry: CgEntry {
                window_id,
                bounds: Bounds::new(10.0, 10.0, 640.0, 480.0),
                title: format!("window-{window_id}").into_boxed_str(),
                onscreen: Some(true),
                z_index,
            },
        }
    }

    #[test]
    fn discovery_prioritizes_cg_z_order_and_preserves_no_window_order() {
        let apps = vec![
            app(10, "frontmost-minimized"),
            app(20, "behind"),
            app(30, "front"),
            app(40, "also-minimized"),
        ];
        let entries = vec![parsed_entry(20, 2, 5), parsed_entry(30, 3, 1)];
        assert_eq!(discovery_app_order(&apps, &entries), vec![2, 1, 0, 3]);
    }

    #[test]
    fn cg_fallback_is_bounded_and_keeps_stable_window_ids() {
        let app = app(42, "bounded");
        let entries = [
            parsed_entry(42, 100, 0),
            parsed_entry(42, 101, 1),
            parsed_entry(42, 102, 2),
        ];
        let refs = entries.iter().collect::<Vec<_>>();
        let windows = synthetic_windows_from_cg(&app, &refs, 2);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].metadata.key.window_id, Some(100));
        assert_eq!(windows[1].metadata.key.window_id, Some(101));
    }

    #[test]
    fn finder_cg_fallback_keeps_every_bounded_existing_window() {
        let mut finder = app(43, "Finder");
        finder.bundle_identifier = Some("com.apple.finder".into());
        let entries = [
            parsed_entry(43, 200, 0),
            parsed_entry(43, 201, 1),
            parsed_entry(43, 202, 2),
        ];
        let refs = entries.iter().collect::<Vec<_>>();
        let windows = synthetic_windows_from_cg(&finder, &refs, 3);
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].metadata.key.window_id, Some(200));
        assert_eq!(windows[1].metadata.key.window_id, Some(201));
        assert_eq!(windows[2].metadata.key.window_id, Some(202));
    }

    #[test]
    fn matches_direct_coordinates_without_vertical_flip() {
        let screens = displays();
        let ax = Bounds::new(100.0, 100.0, 800.0, 600.0);
        let direct = Bounds::new(100.0, 100.0, 800.0, 600.0);
        let flipped = Bounds::new(100.0, 380.0, 800.0, 600.0);
        assert!(match_score(ax, direct, &screens) < 0.001);
        assert!(match_score(ax, flipped, &screens) > MATCH_DISTANCE_LIMIT);
    }

    #[test]
    fn direct_coordinates_choose_the_correct_window() {
        let entries = vec![
            CgEntry {
                window_id: 1,
                bounds: Bounds::new(-1270.0, 120.0, 900.0, 700.0),
                title: "direct".into(),
                onscreen: Some(true),
                z_index: 0,
            },
            CgEntry {
                window_id: 2,
                bounds: Bounds::new(-1270.0, 284.0, 900.0, 700.0),
                title: "would-match-a-flip".into(),
                onscreen: Some(true),
                z_index: 1,
            },
        ];
        let matched = best_unused_match(
            None,
            "direct",
            Bounds::new(-1268.0, 122.0, 900.0, 700.0),
            &entries,
            &[false, false],
            &displays(),
        );
        assert_eq!(matched, Some(0));
    }

    #[test]
    fn focus_repair_prefers_exact_window_id_over_duplicate_arc_titles() {
        assert_eq!(focus_repair_rank(Some(22), "Arc", Some(11), "Arc"), None);
        assert_eq!(focus_repair_rank(Some(22), "Arc", Some(22), "Arc"), Some(0));
        assert_eq!(
            focus_repair_rank(Some(22), "Arc", Some(11), "Different"),
            None
        );
    }

    #[test]
    fn focus_repair_never_uses_an_empty_title_as_identity() {
        assert_eq!(focus_repair_rank(None, "", None, ""), None);
        assert_eq!(focus_repair_rank(Some(7), "", Some(7), ""), Some(0));
    }

    #[test]
    fn quick_switch_validation_requires_the_exact_process_window_pair() {
        let snapshot = LiveWindowSnapshot {
            identities: vec![WindowIdentity::new(10, 7), WindowIdentity::new(20, 8)],
        };
        assert!(snapshot.identities().contains(&WindowIdentity::new(10, 7)));
        assert!(!snapshot.identities().contains(&WindowIdentity::new(20, 7)));
        assert!(!snapshot.identities().contains(&WindowIdentity::new(10, 8)));
    }

    #[test]
    fn bounded_snapshot_lookup_remains_exact_under_stress() {
        let identities = (0..MAX_CG_ENTRIES)
            .map(|index| WindowIdentity::new((index % 31) as i32 + 1, index as u32 + 100))
            .collect::<Vec<_>>();
        let snapshot = LiveWindowSnapshot { identities };
        assert_eq!(snapshot.identities().len(), MAX_CG_ENTRIES);
        for index in 0..MAX_CG_ENTRIES {
            assert!(snapshot.identities().contains(&WindowIdentity::new(
                (index % 31) as i32 + 1,
                index as u32 + 100,
            )));
        }
        assert!(
            !snapshot
                .identities()
                .contains(&WindowIdentity::new(1, u32::MAX))
        );
    }

    #[test]
    fn finder_discovery_uses_window_identity_and_ax_kind_not_documents_or_titles() {
        // A stable CG ID wins even when Finder omits role metadata, as happens
        // for cross-Space gap-fill entries.
        assert!(is_focusable_finder_ax_window(true, None, None));

        // Virtual Finder locations such as Recents and iCloud can omit
        // AXDocument, but remain real standard windows through their retained
        // AX child. A legitimate folder named Desktop follows this same path.
        assert!(is_focusable_finder_ax_window(
            false,
            Some("AXWindow"),
            Some("AXStandardWindow")
        ));
        assert!(is_focusable_finder_ax_window(
            false,
            Some("AXWindow"),
            Some("AXDialog")
        ));

        // Finder's background/internal accessibility elements have no stable
        // ID and are not standard/dialog windows, so they do not become cards.
        assert!(!is_focusable_finder_ax_window(
            false,
            Some("AXWindow"),
            Some("AXUnknown")
        ));
        assert!(!is_focusable_finder_ax_window(
            false,
            Some("AXGroup"),
            Some("AXStandardWindow")
        ));
        assert!(!is_focusable_finder_ax_window(
            true,
            Some("AXGroup"),
            Some("AXStandardWindow")
        ));
        assert!(!is_focusable_finder_ax_window(
            true,
            Some("AXWindow"),
            Some("AXSystemDialog")
        ));
    }

    #[cfg(not(feature = "app-store"))]
    #[test]
    fn exact_window_fronting_symbols_resolve_on_supported_macos() {
        let api = skylight();
        assert!(api.get_process_for_pid.is_some());
        assert!(api.set_front_process.is_some());
        assert!(api.post_event_record.is_some());
    }

    #[cfg(not(feature = "app-store"))]
    #[test]
    fn make_key_records_balance_the_requested_window_off_content() {
        let (down, up) = make_key_event_records(0x1234_5678);
        assert_eq!(down[0x08], 0x01);
        assert_eq!(up[0x08], 0x02);

        for record in [down, up] {
            assert_eq!(record.len(), 0x100);
            assert_eq!(record[0x04], 0xf8);
            assert_eq!(record[0x3a], 0x10);
            assert_eq!(
                u32::from_ne_bytes(record[0x3c..0x40].try_into().unwrap()),
                0x1234_5678
            );
            assert_eq!(
                f64::from_ne_bytes(record[0x20..0x28].try_into().unwrap()),
                300_000.0
            );
            assert_eq!(
                f64::from_ne_bytes(record[0x28..0x30].try_into().unwrap()),
                300_000.0
            );
        }
    }

    #[test]
    fn gap_fill_keeps_named_window_below_fullscreen_threshold_on_other_display() {
        let entry = CgEntry {
            window_id: 70,
            bounds: Bounds::new(-1180.0, 180.0, 760.0, 620.0),
            title: "Project on external display".into(),
            onscreen: Some(false),
            z_index: 0,
        };
        assert!(!entry.bounds.is_fullscreen_like(displays()[1].frame));
        assert!(should_include_unmatched_cg_entry(
            &entry,
            &displays(),
            false
        ));
    }

    #[test]
    fn gap_fill_keeps_untitled_display_sized_window_when_onscreen_is_unknown() {
        let entry = CgEntry {
            window_id: 71,
            bounds: Bounds::new(-1280.0, 80.0, 1280.0, 1024.0),
            title: Box::default(),
            onscreen: None,
            z_index: 0,
        };
        assert!(should_include_unmatched_cg_entry(
            &entry,
            &displays(),
            false
        ));
    }

    #[test]
    fn only_explicitly_onscreen_gap_can_be_geometry_deduplicated() {
        assert!(can_suppress_as_current_space_duplicate(Some(true)));
        assert!(!can_suppress_as_current_space_duplicate(Some(false)));
        assert!(!can_suppress_as_current_space_duplicate(None));
    }

    #[test]
    fn gap_fill_rejects_small_untitled_helper_surface() {
        let entry = CgEntry {
            window_id: 72,
            bounds: Bounds::new(-1180.0, 180.0, 640.0, 480.0),
            title: Box::default(),
            onscreen: None,
            z_index: 0,
        };
        assert!(!should_include_unmatched_cg_entry(
            &entry,
            &displays(),
            false
        ));
    }

    #[test]
    fn finder_gap_fill_keeps_untitled_ordinary_cross_space_windows() {
        let entry = CgEntry {
            window_id: 73,
            bounds: Bounds::new(-1080.0, 220.0, 700.0, 520.0),
            title: Box::default(),
            onscreen: Some(false),
            z_index: 0,
        };
        assert!(!entry.bounds.is_fullscreen_like(displays()[1].frame));
        assert!(should_include_unmatched_cg_entry(&entry, &displays(), true));
        assert!(!should_include_unmatched_cg_entry(
            &entry,
            &displays(),
            false
        ));
    }

    #[test]
    fn matching_is_one_to_one() {
        let entries = vec![
            CgEntry {
                window_id: 1,
                bounds: Bounds::new(0.0, 0.0, 800.0, 600.0),
                title: "one".into(),
                onscreen: Some(true),
                z_index: 0,
            },
            CgEntry {
                window_id: 2,
                bounds: Bounds::new(20.0, 20.0, 800.0, 600.0),
                title: "two".into(),
                onscreen: Some(true),
                z_index: 1,
            },
        ];
        let mut used = vec![false; 2];
        let first = best_unused_match(
            None,
            "one",
            Bounds::new(1.0, 1.0, 800.0, 600.0),
            &entries,
            &used,
            &displays(),
        )
        .unwrap();
        used[first] = true;
        let second = best_unused_match(
            None,
            "two",
            Bounds::new(2.0, 2.0, 800.0, 600.0),
            &entries,
            &used,
            &displays(),
        )
        .unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn exact_ax_window_id_wins_when_space_windows_share_bounds() {
        let entries = vec![
            CgEntry {
                window_id: 41,
                bounds: Bounds::new(0.0, 0.0, 1440.0, 900.0),
                title: "Arc B".into(),
                onscreen: Some(false),
                z_index: 0,
            },
            CgEntry {
                window_id: 40,
                bounds: Bounds::new(0.0, 0.0, 1440.0, 900.0),
                title: "Arc A".into(),
                onscreen: Some(true),
                z_index: 1,
            },
        ];
        let matched = best_unused_match(
            Some(40),
            "stale or unavailable title",
            Bounds::new(0.0, 0.0, 1440.0, 900.0),
            &entries,
            &[false, false],
            &displays(),
        );
        assert_eq!(matched, Some(1));
    }

    #[test]
    fn titles_disambiguate_equal_bounds_independent_of_cg_order() {
        let entries = vec![
            CgEntry {
                window_id: 51,
                bounds: Bounds::new(0.0, 0.0, 1440.0, 900.0),
                title: "Project B — Visual Studio Code".into(),
                onscreen: Some(false),
                z_index: 0,
            },
            CgEntry {
                window_id: 50,
                bounds: Bounds::new(0.0, 0.0, 1440.0, 900.0),
                title: "Project A — Visual Studio Code".into(),
                onscreen: Some(true),
                z_index: 1,
            },
        ];
        let matched = best_unused_match(
            None,
            "Project A — Visual Studio Code",
            Bounds::new(0.0, 0.0, 1440.0, 900.0),
            &entries,
            &[false, false],
            &displays(),
        );
        assert_eq!(matched, Some(1));
    }

    #[test]
    fn ambiguous_equal_bounds_fail_closed_instead_of_swapping_thumbnails() {
        let entries = vec![
            CgEntry {
                window_id: 60,
                bounds: Bounds::new(0.0, 0.0, 1440.0, 900.0),
                title: "Arc".into(),
                onscreen: Some(true),
                z_index: 0,
            },
            CgEntry {
                window_id: 61,
                bounds: Bounds::new(0.0, 0.0, 1440.0, 900.0),
                title: "Arc".into(),
                onscreen: Some(false),
                z_index: 1,
            },
        ];
        let matched = best_unused_match(
            None,
            "Arc",
            Bounds::new(0.0, 0.0, 1440.0, 900.0),
            &entries,
            &[false, false],
            &displays(),
        );
        assert_eq!(matched, None);
    }
}
