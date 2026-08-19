use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use objc2::rc::{Allocated, Retained, Weak};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSBezierPath, NSColor, NSCompositingOperation,
    NSEvent, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSGraphicsContext,
    NSImage, NSPanel, NSPopUpMenuWindowLevel, NSRunningApplication, NSScreen, NSStringDrawing,
    NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState,
    NSVisualEffectView, NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSAttributedStringKey, NSDictionary, NSObject, NSObjectProtocol,
    NSPoint, NSRect, NSSize, NSString,
};

use crate::core::layout::GridMetrics;
use crate::core::model::Bounds;

use super::windows::ManagedWindow;

const MAX_RENDERED_WINDOWS: usize = 192;
const MAX_RENDER_CACHE_ITEMS: usize = MAX_RENDERED_WINDOWS;
const MAX_ICON_CACHE_ITEMS: usize = 64;
const MAX_APP_STRIP_ITEMS: usize = 18;
const PANEL_MAX_WIDTH: f64 = 1_280.0;
const PANEL_MAX_HEIGHT: f64 = 760.0;
const PANEL_SCREEN_INSET_X: f64 = 40.0;
const PANEL_SCREEN_INSET_Y: f64 = 60.0;
const FINDER_BUNDLE_IDENTIFIER: &str = "com.apple.finder";
const APP_STRIP_SPACING: f64 = 42.0;
const APP_STRIP_TOP: f64 = 10.0;
const APP_STRIP_HIT_SIZE: f64 = 38.0;

// Public NSTrackingArea option values. The dependency is intentionally not
// widened just for this tiny runtime-created object; AppKit owns the ABI and
// these values have been stable since tracking areas were introduced.
const TRACKING_MOUSE_ENTERED_AND_EXITED: usize = 0x01;
const TRACKING_MOUSE_MOVED: usize = 0x02;
const TRACKING_ACTIVE_ALWAYS: usize = 0x80;
const TRACKING_IN_VISIBLE_RECT: usize = 0x200;

pub type SelectionCallback = unsafe extern "C" fn(*mut c_void, usize);
pub type ActivationCallback = unsafe extern "C" fn(*mut c_void, usize);
pub type ViewportCallback = unsafe extern "C" fn(*mut c_void);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridNavigation {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CurrentWindow {
    pub pid: i32,
    pub window_id: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AccessibilityPreferences {
    reduce_transparency: bool,
    increase_contrast: bool,
    voice_over: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RenderIdentity {
    pid: i32,
    window_id: Option<u32>,
    fallback_ordinal: u16,
}

impl RenderIdentity {
    fn from_window(window: &ManagedWindow) -> Self {
        Self {
            pid: window.metadata.key.pid,
            window_id: window.metadata.key.window_id,
            fallback_ordinal: window.metadata.key.fallback_ordinal,
        }
    }

    fn is_current(self, current: Option<CurrentWindow>) -> bool {
        current.is_some_and(|current| {
            self.pid == current.pid && self.window_id == Some(current.window_id)
        })
    }
}

#[derive(Clone)]
struct RenderItem {
    identity: RenderIdentity,
    pid: i32,
    window_id: Option<u32>,
    title: Retained<NSString>,
    app_name: Retained<NSString>,
    accessibility_label: Retained<NSString>,
    minimized: bool,
    is_finder: bool,
    is_current: bool,
    fullscreen_like: bool,
    display_badge: Option<Retained<NSString>>,
    icon: Option<Retained<NSImage>>,
    thumbnail: Option<Retained<NSImage>>,
}

struct RenderCacheEntry {
    identity: RenderIdentity,
    source_title: Box<str>,
    source_app_name: Box<str>,
    minimized: bool,
    is_finder: bool,
    fullscreen_like: bool,
    display_number: Option<usize>,
    item: RenderItem,
    touched: u64,
}

struct IconCacheEntry {
    pid: i32,
    icon: Option<Retained<NSImage>>,
    touched: u64,
}

struct RenderModelCache {
    entries: Vec<RenderCacheEntry>,
    icons: Vec<IconCacheEntry>,
    clock: u64,
    position_count: usize,
    position_labels: Vec<Retained<NSString>>,
}

impl Default for RenderModelCache {
    fn default() -> Self {
        Self {
            entries: Vec::with_capacity(MAX_RENDER_CACHE_ITEMS),
            icons: Vec::with_capacity(MAX_ICON_CACHE_ITEMS),
            clock: 0,
            position_count: 0,
            position_labels: Vec::with_capacity(MAX_RENDERED_WINDOWS),
        }
    }
}

impl RenderModelCache {
    fn next_tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    fn icon_for_pid(&mut self, pid: i32) -> Option<Retained<NSImage>> {
        let tick = self.next_tick();
        if let Some(entry) = self.icons.iter_mut().find(|entry| entry.pid == pid) {
            entry.touched = tick;
            return entry.icon.clone();
        }

        let icon = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .and_then(|application| application.icon());
        if self.icons.len() == MAX_ICON_CACHE_ITEMS {
            let index = least_recent_index(self.icons.iter().map(|entry| entry.touched))
                .unwrap_or_default();
            self.icons.swap_remove(index);
        }
        self.icons.push(IconCacheEntry {
            pid,
            icon: icon.clone(),
            touched: tick,
        });
        icon
    }

    fn item_for_window(
        &mut self,
        window: &ManagedWindow,
        display_number: Option<usize>,
        fullscreen_like: bool,
        current: Option<CurrentWindow>,
    ) -> RenderItem {
        let identity = RenderIdentity::from_window(window);
        let source_title = window.metadata.display_title();
        let source_app_name = &*window.metadata.app_name;
        let minimized = window.metadata.minimized;
        let is_finder =
            window.metadata.bundle_identifier.as_deref() == Some(FINDER_BUNDLE_IDENTIFIER);
        let tick = self.next_tick();

        if let Some(entry) = self.entries.iter_mut().find(|entry| {
            entry.identity == identity
                && &*entry.source_title == source_title
                && &*entry.source_app_name == source_app_name
                && entry.minimized == minimized
                && entry.is_finder == is_finder
                && entry.fullscreen_like == fullscreen_like
                && entry.display_number == display_number
        }) {
            entry.touched = tick;
            let mut item = entry.item.clone();
            item.is_current = identity.is_current(current);
            item.thumbnail = None;
            return item;
        }

        let title = ellipsize(source_title, 56);
        let app_name = ellipsize(source_app_name, 38);
        let icon = self.icon_for_pid(identity.pid);
        let item = RenderItem {
            identity,
            pid: identity.pid,
            window_id: identity.window_id,
            title: NSString::from_str(&title),
            app_name: NSString::from_str(&app_name),
            accessibility_label: NSString::from_str(&window_accessibility_label(
                source_title,
                source_app_name,
                display_number,
                fullscreen_like,
                minimized,
            )),
            minimized,
            is_finder,
            is_current: identity.is_current(current),
            fullscreen_like,
            display_badge: display_number
                .map(|display_number| NSString::from_str(&format!("D{display_number}"))),
            icon,
            thumbnail: None,
        };

        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.identity == identity)
        {
            self.entries.swap_remove(index);
        } else if self.entries.len() == MAX_RENDER_CACHE_ITEMS {
            let index = least_recent_index(self.entries.iter().map(|entry| entry.touched))
                .unwrap_or_default();
            self.entries.swap_remove(index);
        }
        self.entries.push(RenderCacheEntry {
            identity,
            source_title: source_title.into(),
            source_app_name: source_app_name.into(),
            minimized,
            is_finder,
            fullscreen_like,
            display_number,
            item: item.clone(),
            touched: tick,
        });
        item
    }

    fn position_labels(&mut self, count: usize) -> &[Retained<NSString>] {
        if self.position_count != count {
            self.position_labels.clear();
            self.position_labels.extend(
                (0..count).map(|index| NSString::from_str(&format!("{} / {count}", index + 1))),
            );
            self.position_count = count;
        }
        &self.position_labels
    }
}

struct RenderState {
    items: Vec<RenderItem>,
    selected: usize,
    scroll_offset: f64,
    app_strip: [usize; MAX_APP_STRIP_ITEMS],
    app_strip_len: usize,
}

impl Default for RenderState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            selected: 0,
            scroll_offset: 0.0,
            app_strip: [usize::MAX; MAX_APP_STRIP_ITEMS],
            app_strip_len: 0,
        }
    }
}

struct AccessibilityCardIvars {
    owner: Weak<SwitcherView>,
    index: usize,
    label: Retained<NSString>,
    identifier: Retained<NSString>,
    frame: Cell<NSRect>,
    selected: Cell<bool>,
    selection_callback: Cell<Option<(SelectionCallback, *mut c_void)>>,
    activation_callback: Cell<Option<(ActivationCallback, *mut c_void)>>,
}

define_class!(
    // SAFETY: NSObject supports subclassing. The child is immutable apart from
    // main-thread Cells and owns only a zeroing weak reference to its view.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = AccessibilityCardIvars]
    struct AccessibilityCard;

    unsafe impl NSObjectProtocol for AccessibilityCard {}

    impl AccessibilityCard {
        #[unsafe(method(isAccessibilityElement))]
        fn is_accessibility_element(&self) -> bool {
            true
        }

        #[unsafe(method_id(accessibilityRole))]
        fn accessibility_role(&self) -> Retained<NSString> {
            NSString::from_str("AXButton")
        }

        #[unsafe(method_id(accessibilityLabel))]
        fn accessibility_label(&self) -> Retained<NSString> {
            self.ivars().label.clone()
        }

        #[unsafe(method_id(accessibilityIdentifier))]
        fn accessibility_identifier(&self) -> Retained<NSString> {
            self.ivars().identifier.clone()
        }

        #[unsafe(method(accessibilityFrame))]
        fn accessibility_frame(&self) -> NSRect {
            self.ivars().frame.get()
        }

        #[unsafe(method_id(accessibilityParent))]
        fn accessibility_parent(&self) -> Option<Retained<AnyObject>> {
            self.ivars().owner.load().map(Into::into)
        }

        #[unsafe(method(isAccessibilitySelected))]
        fn is_accessibility_selected(&self) -> bool {
            self.ivars().selected.get()
        }

        #[unsafe(method(accessibilityPerformPress))]
        fn accessibility_perform_press(&self) -> bool {
            if let Some((callback, context)) = self.ivars().activation_callback.get() {
                // SAFETY: The view clears every child callback before its stable
                // controller context is destroyed or a card is retired.
                unsafe { callback(context, self.ivars().index) };
                true
            } else if let Some((callback, context)) = self.ivars().selection_callback.get() {
                unsafe { callback(context, self.ivars().index) };
                true
            } else {
                false
            }
        }
    }
);

impl AccessibilityCard {
    fn new(
        owner: &SwitcherView,
        index: usize,
        label: Retained<NSString>,
        selection_callback: Option<(SelectionCallback, *mut c_void)>,
        activation_callback: Option<(ActivationCallback, *mut c_void)>,
    ) -> Retained<Self> {
        let mtm = MainThreadMarker::new().expect("accessibility cards are main-thread-only");
        let ivars = AccessibilityCardIvars {
            owner: Weak::new(owner),
            index,
            label,
            identifier: NSString::from_str(&format!("macflow-window-{index}")),
            frame: Cell::new(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0))),
            selected: Cell::new(false),
            selection_callback: Cell::new(selection_callback),
            activation_callback: Cell::new(activation_callback),
        };
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }

    fn clear_callbacks(&self) {
        self.ivars().selection_callback.set(None);
        self.ivars().activation_callback.set(None);
    }
}

struct SwitcherViewIvars {
    state: RefCell<RenderState>,
    render_cache: RefCell<RenderModelCache>,
    callback: Cell<Option<(SelectionCallback, *mut c_void)>>,
    activation_callback: Cell<Option<(ActivationCallback, *mut c_void)>>,
    viewport_callback: Cell<Option<(ViewportCallback, *mut c_void)>>,
    current_window: Cell<Option<CurrentWindow>>,
    last_hovered: Cell<Option<usize>>,
    tracking_area: RefCell<Option<Retained<AnyObject>>>,
    accessibility_cards: RefCell<Vec<Retained<AccessibilityCard>>>,
    accessibility_children: RefCell<Retained<NSArray<AnyObject>>>,
    accessibility_preferences: Cell<AccessibilityPreferences>,
    title_attributes: Retained<NSDictionary<NSAttributedStringKey, AnyObject>>,
    subtitle_attributes: Retained<NSDictionary<NSAttributedStringKey, AnyObject>>,
    muted_attributes: Retained<NSDictionary<NSAttributedStringKey, AnyObject>>,
    high_contrast_title_attributes: Retained<NSDictionary<NSAttributedStringKey, AnyObject>>,
    high_contrast_subtitle_attributes: Retained<NSDictionary<NSAttributedStringKey, AnyObject>>,
    high_contrast_muted_attributes: Retained<NSDictionary<NSAttributedStringKey, AnyObject>>,
    badge_attributes: Retained<NSDictionary<NSAttributedStringKey, AnyObject>>,
    current_badge: Retained<NSString>,
    minimized_badge: Retained<NSString>,
    fullscreen_badge: Retained<NSString>,
}

define_class!(
    // SAFETY: NSView supports subclassing. All state and methods are confined
    // to AppKit's main thread and the subclass itself has no custom Drop impl.
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = SwitcherViewIvars]
    struct SwitcherView;

    unsafe impl NSObjectProtocol for SwitcherView {}

    impl SwitcherView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty_rect: NSRect) {
            self.draw_contents(dirty_rect);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let Some(index) = self
                .app_strip_index_at_point(point)
                .or_else(|| self.index_at_point(point))
            else {
                return;
            };
            if event.clickCount() >= 2
                && let Some((callback, context)) = self.ivars().activation_callback.get()
            {
                // SAFETY: Overlay users clear/order out the view before the
                // controller context is destroyed; mouseDown runs on main.
                unsafe { callback(context, index) };
            } else {
                self.emit_selection(index);
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let hovered = self.index_at_point(point);
            if hovered == self.ivars().last_hovered.get() {
                return;
            }
            self.ivars().last_hovered.set(hovered);
            if let Some(index) = hovered {
                self.emit_selection(index);
            }
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().last_hovered.set(None);
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            unsafe {
                let _: () = msg_send![super(self), updateTrackingAreas];
            }
            self.replace_tracking_area();
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            // The overlay intentionally does not activate or become key. Accept
            // its first click so a card selection is never swallowed by AppKit.
            true
        }

        #[unsafe(method(needsPanelToBecomeKey))]
        fn needs_panel_to_become_key(&self) -> bool {
            false
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let bounds = self.bounds();
            let metrics = GridMetrics::default();
            let mut state = self.ivars().state.borrow_mut();
            let columns = metrics.columns(bounds.size.width);
            let max_offset =
                (metrics.content_height(state.items.len(), columns) - bounds.size.height).max(0.0);
            let scale = if event.hasPreciseScrollingDeltas() {
                1.0
            } else {
                18.0
            };
            let old_offset = state.scroll_offset;
            let old_rows = visible_row_span(metrics, bounds, state.items.len(), old_offset);
            state.scroll_offset =
                (state.scroll_offset - event.scrollingDeltaY() * scale).clamp(0.0, max_offset);
            let changed = (state.scroll_offset - old_offset).abs() > f64::EPSILON;
            let visible_rows_changed =
                old_rows != visible_row_span(metrics, bounds, state.items.len(), state.scroll_offset);
            drop(state);
            if changed {
                self.setNeedsDisplay(true);
                if visible_rows_changed
                    && let Some((callback, context)) = self.ivars().viewport_callback.get()
                {
                    // SAFETY: Overlay clears the callback before its stable
                    // controller context can be released. Notify only when a
                    // row enters/leaves, keeping high-rate trackpad scrolling
                    // off the capture scheduler's synchronization path.
                    unsafe { callback(context) };
                }
                self.update_accessibility_layout();
            }
        }

        #[unsafe(method(isAccessibilityElement))]
        fn is_accessibility_element(&self) -> bool {
            false
        }

        #[unsafe(method_id(accessibilityRole))]
        fn accessibility_role(&self) -> Retained<NSString> {
            NSString::from_str("AXList")
        }

        #[unsafe(method_id(accessibilityLabel))]
        fn accessibility_label(&self) -> Retained<NSString> {
            NSString::from_str("Window chooser")
        }

        #[unsafe(method_id(accessibilityChildren))]
        fn accessibility_children(&self) -> Retained<NSArray<AnyObject>> {
            self.ivars().accessibility_children.borrow().clone()
        }

        #[unsafe(method_id(accessibilitySelectedChildren))]
        fn accessibility_selected_children(&self) -> Retained<NSArray<AnyObject>> {
            let cards = self.ivars().accessibility_cards.borrow();
            let selected = self.ivars().state.borrow().selected;
            cards.get(selected).map_or_else(
                || NSArray::from_slice(&[]),
                |card| {
                    let object: Retained<AnyObject> = card.clone().into();
                    NSArray::from_retained_slice(&[object])
                },
            )
        }
    }
);

impl SwitcherView {
    fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let ivars = SwitcherViewIvars {
            state: RefCell::new(RenderState::default()),
            render_cache: RefCell::new(RenderModelCache::default()),
            callback: Cell::new(None),
            activation_callback: Cell::new(None),
            viewport_callback: Cell::new(None),
            current_window: Cell::new(None),
            last_hovered: Cell::new(None),
            tracking_area: RefCell::new(None),
            accessibility_cards: RefCell::new(Vec::with_capacity(MAX_RENDERED_WINDOWS)),
            accessibility_children: RefCell::new(NSArray::from_slice(&[])),
            accessibility_preferences: Cell::new(AccessibilityPreferences::default()),
            title_attributes: text_attributes(12.5, 0.92),
            subtitle_attributes: text_attributes(10.5, 0.48),
            muted_attributes: text_attributes(12.0, 0.42),
            high_contrast_title_attributes: text_attributes(12.5, 1.0),
            high_contrast_subtitle_attributes: text_attributes(10.5, 0.9),
            high_contrast_muted_attributes: text_attributes(12.0, 0.82),
            badge_attributes: text_attributes(9.5, 0.95),
            current_badge: NSString::from_str("Current"),
            minimized_badge: NSString::from_str("Minimized"),
            fullscreen_badge: NSString::from_str("Full Screen"),
        };
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: This is NSView's designated frame initializer and `this` is
        // a freshly allocated SwitcherView.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn set_callback(&self, callback: SelectionCallback, context: *mut c_void) {
        self.ivars().callback.set(Some((callback, context)));
        for card in self.ivars().accessibility_cards.borrow().iter() {
            card.ivars()
                .selection_callback
                .set(Some((callback, context)));
        }
    }

    fn set_activation_callback(&self, callback: ActivationCallback, context: *mut c_void) {
        self.ivars()
            .activation_callback
            .set(Some((callback, context)));
        for card in self.ivars().accessibility_cards.borrow().iter() {
            card.ivars()
                .activation_callback
                .set(Some((callback, context)));
        }
    }

    fn clear_callback(&self) {
        self.ivars().callback.set(None);
        self.ivars().activation_callback.set(None);
        self.ivars().viewport_callback.set(None);
        for card in self.ivars().accessibility_cards.borrow().iter() {
            card.clear_callbacks();
        }
    }

    fn set_viewport_callback(&self, callback: ViewportCallback, context: *mut c_void) {
        self.ivars()
            .viewport_callback
            .set(Some((callback, context)));
    }

    fn set_current_window(&self, current: Option<CurrentWindow>) {
        self.ivars().current_window.set(current);
        let mut state = self.ivars().state.borrow_mut();
        let fallback = current.or_else(|| {
            state.items.first().and_then(|item| {
                item.window_id.map(|window_id| CurrentWindow {
                    pid: item.pid,
                    window_id,
                })
            })
        });
        for item in &mut state.items {
            item.is_current = item.identity.is_current(fallback);
        }
        drop(state);
        self.setNeedsDisplay(true);
    }

    fn refresh_accessibility_preferences(&self) {
        self.ivars()
            .accessibility_preferences
            .set(system_accessibility_preferences());
    }

    fn emit_selection(&self, index: usize) {
        let item_count = self.ivars().state.borrow().items.len();
        if index >= item_count {
            return;
        }
        if let Some((callback, context)) = self.ivars().callback.get() {
            // SAFETY: Callback lifetime is bounded by Overlay and all pointer
            // events run synchronously on the AppKit main thread.
            unsafe { callback(context, index) };
        }
    }

    fn navigate(&self, direction: GridNavigation) -> Option<usize> {
        let state = self.ivars().state.borrow();
        let columns = GridMetrics::default().columns(self.bounds().size.width);
        let target = grid_navigation_target(state.selected, state.items.len(), columns, direction)?;
        let unchanged = target == state.selected;
        drop(state);
        if !unchanged {
            self.emit_selection(target);
        }
        Some(target)
    }

    fn set_items(&self, windows: &[ManagedWindow], selected: usize) {
        self.ivars().last_hovered.set(None);
        let displays = collect_display_bounds();
        let configured_current = self.ivars().current_window.get();
        let current = configured_current
            .filter(|current| {
                windows.iter().any(|window| {
                    window.metadata.key.pid == current.pid
                        && window.metadata.key.window_id == Some(current.window_id)
                })
            })
            .or_else(|| {
                windows.first().and_then(|window| {
                    window
                        .metadata
                        .key
                        .window_id
                        .map(|window_id| CurrentWindow {
                            pid: window.metadata.key.pid,
                            window_id,
                        })
                })
            });
        let mut items = Vec::with_capacity(windows.len().min(MAX_RENDERED_WINDOWS));
        let mut cache = self.ivars().render_cache.borrow_mut();
        for window in windows.iter().take(MAX_RENDERED_WINDOWS) {
            let profile = display_profile(window.metadata.bounds, &displays);
            items.push(cache.item_for_window(
                window,
                (displays.len() > 1).then_some(profile.display_index + 1),
                profile.fullscreen_like,
                current,
            ));
        }

        // Precompute all renderer-only data once per presentation. Selection
        // changes then redraw without HashSet/Vec/format allocations.
        let mut app_strip = [usize::MAX; MAX_APP_STRIP_ITEMS];
        let mut app_strip_len = 0;
        for (index, item) in items.iter().enumerate() {
            if app_strip[..app_strip_len]
                .iter()
                .any(|existing| items[*existing].pid == item.pid)
            {
                continue;
            }
            app_strip[app_strip_len] = index;
            app_strip_len += 1;
            if app_strip_len == MAX_APP_STRIP_ITEMS {
                break;
            }
        }
        let _ = cache.position_labels(items.len());
        drop(cache);

        let mut state = self.ivars().state.borrow_mut();
        state.items = items;
        state.selected = selected.min(state.items.len().saturating_sub(1));
        state.scroll_offset = 0.0;
        state.app_strip = app_strip;
        state.app_strip_len = app_strip_len;
        drop(state);
        self.rebuild_accessibility_cards();
        self.reveal_selection();
        self.update_accessibility_layout();
        self.setNeedsDisplay(true);
    }

    fn set_selection(&self, selected: usize) {
        let bounds = self.bounds();
        let metrics = GridMetrics::default();
        let mut state = self.ivars().state.borrow_mut();
        if selected >= state.items.len() || selected == state.selected {
            return;
        }
        let old_selected = state.selected;
        let old_offset = state.scroll_offset;
        let columns = metrics.columns(bounds.size.width);
        let new_offset = metrics.reveal_offset(
            selected,
            columns,
            bounds.size.height,
            old_offset,
            state.items.len(),
        );
        let old_card = card_rect(
            metrics,
            old_selected,
            state.items.len(),
            bounds.size.width,
            old_offset,
        );
        let new_card = card_rect(
            metrics,
            selected,
            state.items.len(),
            bounds.size.width,
            new_offset,
        );
        state.selected = selected;
        state.scroll_offset = new_offset;
        drop(state);
        self.update_accessibility_selection(selected);
        if (new_offset - old_offset).abs() > f64::EPSILON {
            self.update_accessibility_layout();
        }
        if (new_offset - old_offset).abs() > f64::EPSILON {
            self.setNeedsDisplay(true);
        } else {
            self.setNeedsDisplayInRect(NSRect::new(
                bounds.origin,
                NSSize::new(bounds.size.width, metrics.top_inset),
            ));
            self.setNeedsDisplayInRect(expand_rect(old_card, 2.0));
            self.setNeedsDisplayInRect(expand_rect(new_card, 2.0));
        }
    }

    pub fn set_thumbnail(&self, window_id: u32, image: Retained<NSImage>) -> bool {
        let bounds = self.bounds();
        let metrics = GridMetrics::default();
        let mut state = self.ivars().state.borrow_mut();
        let Some(index) = state
            .items
            .iter()
            .position(|item| item.window_id == Some(window_id))
        else {
            return false;
        };
        state.items[index].thumbnail = Some(image);
        let dirty = card_rect(
            metrics,
            index,
            state.items.len(),
            bounds.size.width,
            state.scroll_offset,
        );
        drop(state);
        self.setNeedsDisplayInRect(expand_rect(dirty, 2.0));
        true
    }

    fn has_thumbnail(&self, window_id: u32) -> bool {
        self.ivars()
            .state
            .borrow()
            .items
            .iter()
            .any(|item| item.window_id == Some(window_id) && item.thumbnail.is_some())
    }

    fn retain_thumbnails(&self, window_ids: &[u32]) {
        let bounds = self.bounds();
        let metrics = GridMetrics::default();
        let viewport = grid_viewport(bounds, metrics);
        let mut dirty = None;
        let mut state = self.ivars().state.borrow_mut();
        let item_count = state.items.len();
        let scroll_offset = state.scroll_offset;
        for (index, item) in state.items.iter_mut().enumerate() {
            let card = card_rect(metrics, index, item_count, bounds.size.width, scroll_offset);
            if item.thumbnail.is_some()
                && !item
                    .window_id
                    .is_some_and(|window_id| window_ids.contains(&window_id))
                // A smooth trackpad offset can expose parts of four rows even
                // though a capture batch holds three full rows. Never erase a
                // thumbnail while any part of its card is actually onscreen.
                && !rects_intersect(card, viewport)
            {
                item.thumbnail = None;
                dirty = Some(dirty.map_or(card, |existing| union_rect(existing, card)));
            }
        }
        drop(state);
        if let Some(dirty) = dirty {
            self.setNeedsDisplayInRect(expand_rect(dirty, 2.0));
        }
    }

    fn thumbnail_indexes(&self, output: &mut [usize]) -> usize {
        let bounds = self.bounds();
        let metrics = GridMetrics::default();
        let state = self.ivars().state.borrow();
        fill_thumbnail_indexes(
            metrics,
            bounds,
            state.items.len(),
            state.selected,
            state.scroll_offset,
            |index| state.items[index].thumbnail.is_some(),
            output,
        )
    }

    fn reveal_selection(&self) {
        let bounds = self.bounds();
        let mut state = self.ivars().state.borrow_mut();
        let metrics = GridMetrics::default();
        let columns = metrics.columns(bounds.size.width);
        state.scroll_offset = metrics.reveal_offset(
            state.selected,
            columns,
            bounds.size.height,
            state.scroll_offset,
            state.items.len(),
        );
    }

    fn index_at_point(&self, point: NSPoint) -> Option<usize> {
        let bounds = self.bounds();
        let metrics = GridMetrics::default();
        if !point_in_rect(point, grid_viewport(bounds, metrics)) {
            return None;
        }
        let state = self.ivars().state.borrow();
        for index in 0..state.items.len() {
            let (x, y) = metrics.card_origin(
                index,
                state.items.len(),
                bounds.size.width,
                state.scroll_offset,
            );
            if point_in_rect(
                point,
                NSRect::new(
                    NSPoint::new(x, y),
                    NSSize::new(metrics.card_width, metrics.card_height),
                ),
            ) {
                return Some(index);
            }
        }
        None
    }

    fn app_strip_index_at_point(&self, point: NSPoint) -> Option<usize> {
        let state = self.ivars().state.borrow();
        for (slot, item_index) in state.app_strip[..state.app_strip_len].iter().enumerate() {
            if point_in_rect(
                point,
                app_strip_hit_rect(slot, state.app_strip_len, self.bounds().size.width),
            ) {
                return Some(*item_index);
            }
        }
        None
    }

    fn replace_tracking_area(&self) {
        if let Some(old) = self.ivars().tracking_area.borrow_mut().take() {
            unsafe {
                let _: () = msg_send![self, removeTrackingArea: &*old];
            }
        }
        let Some(class) = AnyClass::get(c"NSTrackingArea") else {
            return;
        };
        let allocated: Allocated<AnyObject> = unsafe { msg_send![class, alloc] };
        let options = TRACKING_MOUSE_ENTERED_AND_EXITED
            | TRACKING_MOUSE_MOVED
            | TRACKING_ACTIVE_ALWAYS
            | TRACKING_IN_VISIBLE_RECT;
        let area: Retained<AnyObject> = unsafe {
            msg_send![allocated,
                initWithRect: self.bounds(),
                options: options,
                owner: self,
                userInfo: Option::<&AnyObject>::None
            ]
        };
        unsafe {
            let _: () = msg_send![self, addTrackingArea: &*area];
        }
        self.ivars().tracking_area.replace(Some(area));
    }

    fn rebuild_accessibility_cards(&self) {
        {
            let cards = self.ivars().accessibility_cards.borrow();
            for card in cards.iter() {
                card.clear_callbacks();
            }
        }

        if !self.ivars().accessibility_preferences.get().voice_over {
            self.ivars().accessibility_cards.borrow_mut().clear();
            self.ivars()
                .accessibility_children
                .replace(NSArray::from_slice(&[]));
            return;
        }

        let selection_callback = self.ivars().callback.get();
        let activation_callback = self.ivars().activation_callback.get();
        let state = self.ivars().state.borrow();
        let mut cards = Vec::with_capacity(state.items.len());
        for (index, item) in state.items.iter().enumerate() {
            cards.push(AccessibilityCard::new(
                self,
                index,
                item.accessibility_label.clone(),
                selection_callback,
                activation_callback,
            ));
        }
        drop(state);

        let children = cards
            .iter()
            .cloned()
            .map(Into::<Retained<AnyObject>>::into)
            .collect::<Vec<_>>();
        self.ivars()
            .accessibility_children
            .replace(NSArray::from_retained_slice(&children));
        self.ivars().accessibility_cards.replace(cards);
        self.update_accessibility_selection(self.ivars().state.borrow().selected);
    }

    fn update_accessibility_selection(&self, selected: usize) {
        for (index, card) in self.ivars().accessibility_cards.borrow().iter().enumerate() {
            card.ivars().selected.set(index == selected);
        }
    }

    fn update_accessibility_layout(&self) {
        let cards = self.ivars().accessibility_cards.borrow();
        if cards.is_empty() {
            return;
        }
        let state = self.ivars().state.borrow();
        let metrics = GridMetrics::default();
        let window = self.window();
        for (index, card) in cards.iter().enumerate() {
            let rect = card_rect(
                metrics,
                index,
                state.items.len(),
                self.bounds().size.width,
                state.scroll_offset,
            );
            let window_rect = self.convertRect_toView(rect, None);
            let screen_rect = window.as_ref().map_or(window_rect, |window| {
                window.convertRectToScreen(window_rect)
            });
            card.ivars().frame.set(screen_rect);
        }
    }

    fn draw_contents(&self, dirty_rect: NSRect) {
        let bounds = self.bounds();
        let preferences = self.ivars().accessibility_preferences.get();
        // Reduce Transparency keeps the Liquid Glass container alive while an
        // opaque content surface prevents background detail shining through.
        fill_rect(
            bounds,
            0.015,
            0.025,
            0.05,
            if preferences.reduce_transparency {
                0.98
            } else {
                0.18
            },
        );

        let state = self.ivars().state.borrow();
        if state.items.is_empty() {
            return;
        }

        let metrics = GridMetrics::default();
        let header = NSRect::new(
            bounds.origin,
            NSSize::new(bounds.size.width, metrics.top_inset),
        );
        if rects_intersect(dirty_rect, header) {
            let selected = &state.items[state.selected];
            let (title_attributes, muted_attributes) = if preferences.increase_contrast {
                (
                    &self.ivars().high_contrast_title_attributes,
                    &self.ivars().high_contrast_muted_attributes,
                )
            } else {
                (
                    &self.ivars().title_attributes,
                    &self.ivars().muted_attributes,
                )
            };
            draw_text_at(
                &selected.app_name,
                NSPoint::new(40.0, 48.0),
                muted_attributes,
            );
            draw_text_in(
                &selected.title,
                NSRect::new(
                    NSPoint::new(40.0, 69.0),
                    NSSize::new((bounds.size.width - 150.0).max(120.0), 24.0),
                ),
                title_attributes,
            );
            let count = self
                .ivars()
                .render_cache
                .borrow()
                .position_labels
                .get(state.selected)
                .cloned();
            if let Some(count) = count {
                draw_text_at(
                    &count,
                    NSPoint::new((bounds.size.width - 86.0).max(40.0), 69.0),
                    muted_attributes,
                );
            }
            self.draw_app_strip(&state, bounds.size.width, preferences);
        }
        let viewport = grid_viewport(bounds, metrics);
        NSGraphicsContext::saveGraphicsState_class();
        NSBezierPath::clipRect(viewport);
        for (index, item) in state.items.iter().enumerate() {
            let (x, y) = metrics.card_origin(
                index,
                state.items.len(),
                bounds.size.width,
                state.scroll_offset,
            );
            let card = NSRect::new(
                NSPoint::new(x, y),
                NSSize::new(metrics.card_width, metrics.card_height),
            );
            if !rects_intersect(card, viewport) || !rects_intersect(card, dirty_rect) {
                continue;
            }
            self.draw_card(item, index == state.selected, x, y, metrics, preferences);
        }
        NSGraphicsContext::restoreGraphicsState_class();
    }

    fn draw_app_strip(
        &self,
        state: &RenderState,
        width: f64,
        preferences: AccessibilityPreferences,
    ) {
        let start_x = app_strip_start_x(state.app_strip_len, width);
        let selected_pid = state.items[state.selected].pid;
        for (index, item_index) in state.app_strip[..state.app_strip_len].iter().enumerate() {
            let item = &state.items[*item_index];
            let active = item.pid == selected_pid;
            let size = if active { 34.0 } else { 30.0 };
            let x = start_x + index as f64 * APP_STRIP_SPACING + (34.0 - size) * 0.5;
            if let Some(icon) = &item.icon {
                draw_image(
                    icon,
                    NSRect::new(NSPoint::new(x, 12.0), NSSize::new(size, size)),
                    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
                    if active || preferences.increase_contrast {
                        1.0
                    } else {
                        0.48
                    },
                );
            } else {
                fill_rounded(
                    NSRect::new(NSPoint::new(x, 12.0), NSSize::new(size, size)),
                    7.0,
                    0.35,
                    0.4,
                    0.5,
                    if active || preferences.increase_contrast {
                        0.9
                    } else {
                        0.42
                    },
                );
            }
        }
    }

    fn draw_card(
        &self,
        item: &RenderItem,
        selected: bool,
        x: f64,
        y: f64,
        metrics: GridMetrics,
        preferences: AccessibilityPreferences,
    ) {
        let card = NSRect::new(
            NSPoint::new(x, y),
            NSSize::new(metrics.card_width, metrics.card_height),
        );
        fill_rounded(
            card,
            14.0,
            0.14,
            0.17,
            0.23,
            if preferences.reduce_transparency || preferences.increase_contrast {
                if selected { 1.0 } else { 0.92 }
            } else if selected {
                0.94
            } else {
                0.72
            },
        );

        let preview = NSRect::new(NSPoint::new(x, y), NSSize::new(metrics.card_width, 168.0));
        // Keep unlike window shapes visually anchored to the middle of their
        // cards.  Aspect-fill made a narrow or short window look offset by
        // throwing away most of one axis; a centered aspect-fit keeps the
        // complete window visible and leaves equal glass-backed margins.
        fill_rounded(
            preview,
            12.0,
            0.08,
            0.11,
            0.17,
            if preferences.reduce_transparency {
                1.0
            } else {
                0.72
            },
        );
        if let Some(thumbnail) = &item.thumbnail {
            NSGraphicsContext::saveGraphicsState_class();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(preview, 12.0, 12.0).addClip();
            draw_image(
                thumbnail,
                aspect_fit_destination(thumbnail.size(), preview),
                NSRect::new(NSPoint::new(0.0, 0.0), thumbnail.size()),
                if selected { 1.0 } else { 0.78 },
            );
            NSGraphicsContext::restoreGraphicsState_class();
        } else if let Some(icon) = &item.icon {
            draw_image(
                icon,
                centered_rect(NSSize::new(64.0, 64.0), preview),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
                if selected { 1.0 } else { 0.72 },
            );
        }

        // Finder windows often have similar folder thumbnails and short
        // titles. A small translucent Finder badge makes them scannable at a
        // glance without obscuring the captured window or allocating another
        // image—the existing per-process icon is reused.
        if item.is_finder
            && let Some(icon) = &item.icon
        {
            let badge = finder_badge_rect(preview);
            fill_rounded(
                badge,
                9.0,
                0.025,
                0.035,
                0.06,
                if selected { 0.68 } else { 0.5 },
            );
            draw_image(
                icon,
                NSRect::new(
                    NSPoint::new(badge.origin.x + 3.0, badge.origin.y + 3.0),
                    NSSize::new(badge.size.width - 6.0, badge.size.height - 6.0),
                ),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
                if selected { 0.92 } else { 0.76 },
            );
        }

        self.draw_card_badges(item, preview, preferences);

        let (title_attributes, subtitle_attributes) = if preferences.increase_contrast {
            (
                &self.ivars().high_contrast_title_attributes,
                &self.ivars().high_contrast_subtitle_attributes,
            )
        } else {
            (
                &self.ivars().title_attributes,
                &self.ivars().subtitle_attributes,
            )
        };
        draw_text_in(
            &item.title,
            NSRect::new(
                NSPoint::new(x + 10.0, y + 177.0),
                NSSize::new(metrics.card_width - 20.0, 18.0),
            ),
            title_attributes,
        );
        draw_text_in(
            &item.app_name,
            NSRect::new(
                NSPoint::new(x + 10.0, y + 196.0),
                NSSize::new(metrics.card_width - 20.0, 15.0),
            ),
            subtitle_attributes,
        );

        stroke_rounded(
            card,
            14.0,
            if selected && preferences.increase_contrast {
                3.5
            } else if selected {
                2.5
            } else {
                1.0
            },
            1.0,
            1.0,
            1.0,
            if selected {
                0.9
            } else if preferences.increase_contrast {
                0.32
            } else {
                0.11
            },
        );
    }

    fn draw_card_badges(
        &self,
        item: &RenderItem,
        preview: NSRect,
        preferences: AccessibilityPreferences,
    ) {
        let mut right = preview.origin.x + preview.size.width - 8.0;
        let alpha = if preferences.increase_contrast {
            0.96
        } else {
            0.78
        };
        let mut draw = |label: &NSString, width: f64| {
            right -= width;
            let rect = NSRect::new(
                NSPoint::new(right, preview.origin.y + 8.0),
                NSSize::new(width, 18.0),
            );
            fill_rounded(rect, 7.0, 0.025, 0.035, 0.06, alpha);
            draw_text_in(
                label,
                NSRect::new(
                    NSPoint::new(rect.origin.x + 6.0, rect.origin.y + 3.0),
                    NSSize::new((rect.size.width - 12.0).max(0.0), 13.0),
                ),
                &self.ivars().badge_attributes,
            );
            right -= 5.0;
        };

        if item.is_current {
            draw(&self.ivars().current_badge, 52.0);
        }
        if item.minimized {
            draw(&self.ivars().minimized_badge, 66.0);
        } else if item.fullscreen_like {
            draw(&self.ivars().fullscreen_badge, 72.0);
        }
        if let Some(display_badge) = &item.display_badge {
            draw(display_badge, 28.0);
        }
    }
}

pub struct Overlay {
    window: Retained<NSPanel>,
    view: Retained<SwitcherView>,
}

impl Overlay {
    pub fn new(mtm: MainThreadMarker) -> Self {
        let zero = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        let window = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            zero,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: This Rust owner keeps the NSWindow retained for the lifetime
        // of the overlay, so AppKit must not release it when ordered out.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setOpaque(false);
        window.setHasShadow(true);
        window.setIgnoresMouseEvents(false);
        window.setAcceptsMouseMovedEvents(true);
        window.setBecomesKeyOnlyIfNeeded(true);
        // This is transient chooser UI, not a screen saver. Pop-up level keeps
        // it above fullscreen app content without taking the system overlay
        // tier used by macOS menu-bar and screen-lock presentation machinery.
        window.setLevel(NSPopUpMenuWindowLevel);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary,
        );

        let view = SwitcherView::new(zero, mtm);
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let backdrop = make_glass_backdrop(zero, &view, mtm);
        window.setContentView(Some(&backdrop));

        Self { window, view }
    }

    pub fn set_callback(&self, callback: SelectionCallback, context: *mut c_void) {
        self.view.set_callback(callback, context);
    }

    /// Installs the direct-activation hook used by double-click and VoiceOver.
    /// The callback receives the same stable window index as SelectionCallback.
    pub fn set_activation_callback(&self, callback: ActivationCallback, context: *mut c_void) {
        self.view.set_activation_callback(callback, context);
    }

    pub fn set_viewport_callback(&self, callback: ViewportCallback, context: *mut c_void) {
        self.view.set_viewport_callback(callback, context);
    }

    /// Supplies the exact origin for the "Current" badge. Until the controller
    /// wires this, the first item remains the safe presentation-order fallback.
    pub fn set_current_window(&self, current: Option<CurrentWindow>) {
        self.view.set_current_window(current);
    }

    /// Moves spatially through the rendered grid and emits SelectionCallback.
    /// This is intentionally separate from the event tap so the controller can
    /// decide which physical keys should own these public navigation actions.
    pub fn navigate(&self, direction: GridNavigation) -> Option<usize> {
        self.view.navigate(direction)
    }

    pub fn show(
        &self,
        frame: NSRect,
        windows: &[ManagedWindow],
        selected: usize,
        should_order_front: impl FnOnce() -> bool,
    ) -> bool {
        let frame = compact_panel_frame(frame);
        self.window.setFrame_display(frame, false);
        self.view
            .setFrame(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
        self.view.refresh_accessibility_preferences();
        self.view.set_items(windows, selected);
        // Preparing the card strings and icons can take a few milliseconds.
        // Re-check the live gesture immediately before the WindowServer order
        // call so a release during preparation cannot flash a stale panel.
        if !should_order_front() {
            return false;
        }
        self.window.setAlphaValue(1.0);
        self.window.orderFrontRegardless();
        true
    }

    /// Replaces cards in an already-visible panel without reordering or
    /// resizing the window. This lets cached content appear immediately while
    /// a fresh discovery snapshot is applied in place.
    pub fn update(&self, windows: &[ManagedWindow], selected: usize) {
        self.view.refresh_accessibility_preferences();
        self.view.set_items(windows, selected);
    }

    pub fn set_selection(&self, selected: usize) {
        self.view.set_selection(selected);
    }

    pub fn set_thumbnail(&self, window_id: u32, image: Retained<NSImage>) -> bool {
        self.view.set_thumbnail(window_id, image)
    }

    pub fn has_thumbnail(&self, window_id: u32) -> bool {
        self.view.has_thumbnail(window_id)
    }

    pub fn retain_thumbnails(&self, window_ids: &[u32]) {
        self.view.retain_thumbnails(window_ids);
    }

    /// Writes the selected card and the cards intersecting the actual scrolled
    /// viewport into `output`. Missing visible thumbnails are ordered first so
    /// a bounded capture batch always spends its capacity on newly exposed
    /// cards instead of invisible neighbours.
    pub fn thumbnail_indexes(&self, output: &mut [usize]) -> usize {
        self.view.thumbnail_indexes(output)
    }

    pub fn hide(&self) {
        self.window.orderOut(None);
    }

    pub fn clear(&self) {
        self.view.set_items(&[], 0);
    }
}

fn make_glass_backdrop(
    frame: NSRect,
    content: &SwitcherView,
    mtm: MainThreadMarker,
) -> Retained<NSView> {
    // NSGlassEffectView is new in macOS 26. Runtime lookup preserves the app's
    // macOS 14 launch floor and avoids a hard class-symbol dependency.
    if let Some(class) = AnyClass::get(c"NSGlassEffectView") {
        let object: Retained<AnyObject> = unsafe { msg_send![class, new] };
        if let Ok(glass) = object.downcast::<NSView>() {
            let tint = NSColor::colorWithSRGBRed_green_blue_alpha(0.12, 0.2, 0.38, 0.16);
            // SAFETY: Runtime class lookup succeeded. These selectors are the
            // macOS 26 NSGlassEffectView API, and every argument has the exact
            // Objective-C ABI representation required by that API.
            unsafe {
                let _: () = msg_send![&glass, setFrame: frame];
                let _: () = msg_send![&glass, setStyle: 0_isize];
                let _: () = msg_send![&glass, setCornerRadius: 28.0_f64];
                let _: () = msg_send![&glass, setTintColor: &*tint];
                let _: () = msg_send![&glass, setContentView: content];
            }
            glass.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
            return glass;
        }
    }

    let blur: Retained<NSVisualEffectView> =
        unsafe { msg_send![NSVisualEffectView::alloc(mtm), initWithFrame: frame] };
    blur.setMaterial(NSVisualEffectMaterial::HUDWindow);
    blur.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    blur.setState(NSVisualEffectState::Active);
    blur.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    blur.addSubview(content);
    blur.downcast::<NSView>()
        .expect("NSVisualEffectView is an NSView subclass")
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.view.clear_callback();
        self.window.orderOut(None);
    }
}

fn system_accessibility_preferences() -> AccessibilityPreferences {
    let workspace = NSWorkspace::sharedWorkspace();
    // These are public NSWorkspace accessibility-display selectors available
    // on every supported macOS release. Raw messaging avoids enabling a large
    // protocol-only binding feature in Cargo just to read three booleans.
    unsafe {
        AccessibilityPreferences {
            reduce_transparency: msg_send![
                &*workspace,
                accessibilityDisplayShouldReduceTransparency
            ],
            increase_contrast: msg_send![&*workspace, accessibilityDisplayShouldIncreaseContrast],
            voice_over: msg_send![&*workspace, isVoiceOverEnabled],
        }
    }
}

fn least_recent_index(touched: impl IntoIterator<Item = u64>) -> Option<usize> {
    touched
        .into_iter()
        .enumerate()
        .min_by_key(|(_, touched)| *touched)
        .map(|(index, _)| index)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DisplayProfile {
    display_index: usize,
    fullscreen_like: bool,
}

fn collect_display_bounds() -> Vec<Bounds> {
    let mtm = MainThreadMarker::new().expect("display lookup is main-thread-only");
    let screens = NSScreen::screens(mtm);
    let primary_frame = screens.firstObject().map_or_else(
        || NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
        |screen| screen.frame(),
    );
    let primary_max_y = primary_frame.origin.y + primary_frame.size.height;
    screens
        .iter()
        .map(|screen| {
            let frame = screen.frame();
            Bounds::new(
                frame.origin.x,
                primary_max_y - (frame.origin.y + frame.size.height),
                frame.size.width,
                frame.size.height,
            )
        })
        .collect()
}

fn display_profile(window: Bounds, displays: &[Bounds]) -> DisplayProfile {
    if displays.is_empty() {
        return DisplayProfile::default();
    }
    let display_index = displays
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| {
            bounds_intersection_area(window, **left)
                .total_cmp(&bounds_intersection_area(window, **right))
        })
        .map_or(0, |(index, _)| index);
    DisplayProfile {
        display_index,
        fullscreen_like: window.is_fullscreen_like(displays[display_index]),
    }
}

fn bounds_intersection_area(left: Bounds, right: Bounds) -> f64 {
    let width = (left.x + left.width).min(right.x + right.width) - left.x.max(right.x);
    let height = (left.y + left.height).min(right.y + right.height) - left.y.max(right.y);
    width.max(0.0) * height.max(0.0)
}

fn window_accessibility_label(
    title: &str,
    app_name: &str,
    display_number: Option<usize>,
    fullscreen_like: bool,
    minimized: bool,
) -> String {
    let title = if title.trim().is_empty() {
        app_name
    } else {
        title
    };
    let mut label = if title == app_name {
        app_name.to_owned()
    } else {
        format!("{title}, {app_name}")
    };
    if let Some(display_number) = display_number {
        label.push_str(&format!(", display {display_number}"));
    }
    if minimized {
        label.push_str(", minimized");
    } else if fullscreen_like {
        label.push_str(", full screen");
    }
    label
}

fn grid_navigation_target(
    selected: usize,
    item_count: usize,
    columns: usize,
    direction: GridNavigation,
) -> Option<usize> {
    if item_count == 0 {
        return None;
    }
    let selected = selected.min(item_count - 1);
    let columns = columns.max(1);
    let row_start = selected / columns * columns;
    let row_end = (row_start + columns).min(item_count) - 1;
    Some(match direction {
        GridNavigation::Left => selected.saturating_sub(1).max(row_start),
        GridNavigation::Right => (selected + 1).min(row_end),
        GridNavigation::Up => selected.checked_sub(columns).unwrap_or(selected),
        GridNavigation::Down => (selected + columns).min(item_count - 1),
        GridNavigation::Home => 0,
        GridNavigation::End => item_count - 1,
    })
}

fn app_strip_start_x(item_count: usize, width: f64) -> f64 {
    ((width - item_count as f64 * APP_STRIP_SPACING) * 0.5).max(40.0)
}

fn app_strip_hit_rect(slot: usize, item_count: usize, width: f64) -> NSRect {
    NSRect::new(
        NSPoint::new(
            app_strip_start_x(item_count, width) + slot as f64 * APP_STRIP_SPACING - 2.0,
            APP_STRIP_TOP,
        ),
        NSSize::new(APP_STRIP_HIT_SIZE, APP_STRIP_HIT_SIZE),
    )
}

fn text_attributes(
    size: f64,
    alpha: f64,
) -> Retained<NSDictionary<NSAttributedStringKey, AnyObject>> {
    let font: Retained<AnyObject> = NSFont::systemFontOfSize(size).into();
    let color: Retained<AnyObject> =
        NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, alpha).into();
    NSDictionary::from_retained_objects(
        &[unsafe { NSFontAttributeName }, unsafe {
            NSForegroundColorAttributeName
        }],
        &[font, color],
    )
}

fn draw_text_at(
    text: &NSString,
    point: NSPoint,
    attributes: &NSDictionary<NSAttributedStringKey, AnyObject>,
) {
    // SAFETY: The dictionary is permanently typed to Cocoa text-attribute
    // keys and contains only NSFont/NSColor values created above.
    unsafe { text.drawAtPoint_withAttributes(point, Some(attributes)) };
}

fn draw_text_in(
    text: &NSString,
    rect: NSRect,
    attributes: &NSDictionary<NSAttributedStringKey, AnyObject>,
) {
    // SAFETY: The dictionary is permanently typed to Cocoa text-attribute
    // keys and contains only NSFont/NSColor values created above.
    unsafe { text.drawInRect_withAttributes(rect, Some(attributes)) };
}

fn draw_image(image: &NSImage, destination: NSRect, source: NSRect, alpha: f64) {
    // SAFETY: `None` supplies no generic hint dictionary. Respecting the
    // flipped drawing context is required for this flipped NSView; the older
    // convenience method would render images upside-down.
    unsafe {
        image.drawInRect_fromRect_operation_fraction_respectFlipped_hints(
            destination,
            source,
            NSCompositingOperation::SourceOver,
            alpha,
            true,
            None,
        );
    }
}

fn finder_badge_rect(preview: NSRect) -> NSRect {
    NSRect::new(
        NSPoint::new(preview.origin.x + 10.0, preview.origin.y + 10.0),
        NSSize::new(34.0, 34.0),
    )
}

fn fill_rect(rect: NSRect, red: f64, green: f64, blue: f64, alpha: f64) {
    NSColor::colorWithSRGBRed_green_blue_alpha(red, green, blue, alpha).setFill();
    NSBezierPath::fillRect(rect);
}

#[allow(clippy::too_many_arguments)]
fn fill_rounded(rect: NSRect, radius: f64, red: f64, green: f64, blue: f64, alpha: f64) {
    NSColor::colorWithSRGBRed_green_blue_alpha(red, green, blue, alpha).setFill();
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius).fill();
}

#[allow(clippy::too_many_arguments)]
fn stroke_rounded(
    rect: NSRect,
    radius: f64,
    width: f64,
    red: f64,
    green: f64,
    blue: f64,
    alpha: f64,
) {
    NSColor::colorWithSRGBRed_green_blue_alpha(red, green, blue, alpha).setStroke();
    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius);
    path.setLineWidth(width);
    path.stroke();
}

fn point_in_rect(point: NSPoint, rect: NSRect) -> bool {
    point.x >= rect.origin.x
        && point.x <= rect.origin.x + rect.size.width
        && point.y >= rect.origin.y
        && point.y <= rect.origin.y + rect.size.height
}

fn rects_intersect(left: NSRect, right: NSRect) -> bool {
    left.origin.x < right.origin.x + right.size.width
        && left.origin.x + left.size.width > right.origin.x
        && left.origin.y < right.origin.y + right.size.height
        && left.origin.y + left.size.height > right.origin.y
}

fn union_rect(left: NSRect, right: NSRect) -> NSRect {
    let min_x = left.origin.x.min(right.origin.x);
    let min_y = left.origin.y.min(right.origin.y);
    let max_x = (left.origin.x + left.size.width).max(right.origin.x + right.size.width);
    let max_y = (left.origin.y + left.size.height).max(right.origin.y + right.size.height);
    NSRect::new(
        NSPoint::new(min_x, min_y),
        NSSize::new(max_x - min_x, max_y - min_y),
    )
}

fn expand_rect(rect: NSRect, amount: f64) -> NSRect {
    NSRect::new(
        NSPoint::new(rect.origin.x - amount, rect.origin.y - amount),
        NSSize::new(
            rect.size.width + amount * 2.0,
            rect.size.height + amount * 2.0,
        ),
    )
}

fn card_rect(
    metrics: GridMetrics,
    index: usize,
    item_count: usize,
    width: f64,
    scroll_offset: f64,
) -> NSRect {
    let (x, y) = metrics.card_origin(index, item_count, width, scroll_offset);
    NSRect::new(
        NSPoint::new(x, y),
        NSSize::new(metrics.card_width, metrics.card_height),
    )
}

fn grid_viewport(bounds: NSRect, metrics: GridMetrics) -> NSRect {
    NSRect::new(
        NSPoint::new(bounds.origin.x, bounds.origin.y + metrics.top_inset),
        NSSize::new(
            bounds.size.width,
            (bounds.size.height - metrics.top_inset - metrics.bottom_inset).max(0.0),
        ),
    )
}

fn visible_row_span(
    metrics: GridMetrics,
    bounds: NSRect,
    item_count: usize,
    scroll_offset: f64,
) -> Option<(usize, usize)> {
    let columns = metrics.columns(bounds.size.width);
    let rows = metrics.rows(item_count, columns);
    let viewport = grid_viewport(bounds, metrics);
    let viewport_bottom = viewport.origin.y + viewport.size.height;
    let mut first = None;
    let mut last_exclusive = 0;
    for row in 0..rows {
        let top = metrics.top_inset + row as f64 * (metrics.card_height + metrics.vertical_spacing)
            - scroll_offset;
        if top + metrics.card_height > viewport.origin.y && top < viewport_bottom {
            first.get_or_insert(row);
            last_exclusive = row + 1;
        }
    }
    first.map(|first| (first, last_exclusive))
}

fn fill_thumbnail_indexes(
    metrics: GridMetrics,
    bounds: NSRect,
    item_count: usize,
    selected: usize,
    scroll_offset: f64,
    has_thumbnail: impl Fn(usize) -> bool,
    output: &mut [usize],
) -> usize {
    if output.is_empty() || item_count == 0 {
        return 0;
    }

    let selected = selected.min(item_count - 1);
    let viewport = grid_viewport(bounds, metrics);
    let mut len = 0;
    push_unique_index(output, &mut len, selected);

    // When a trackpad stops between row boundaries, four partial/full rows can
    // intersect the viewport. Fill missing cards first; thumbnails already
    // held by the view are independently retained while onscreen.
    for wants_missing in [true, false] {
        for index in 0..item_count {
            if wants_missing == has_thumbnail(index) {
                continue;
            }
            let card = card_rect(metrics, index, item_count, bounds.size.width, scroll_offset);
            if rects_intersect(card, viewport) {
                push_unique_index(output, &mut len, index);
                if len == output.len() {
                    return len;
                }
            }
        }
    }

    // Use any spare capacity to prewarm the closest cards without displacing
    // the viewport. This preserves the previous low-latency cycling behaviour.
    for distance in 1..item_count {
        if let Some(left) = selected.checked_sub(distance) {
            push_unique_index(output, &mut len, left);
        }
        if let Some(right) = selected.checked_add(distance)
            && right < item_count
        {
            push_unique_index(output, &mut len, right);
        }
        if len == output.len() {
            break;
        }
    }
    len
}

fn push_unique_index(output: &mut [usize], len: &mut usize, index: usize) {
    if *len < output.len() && !output[..*len].contains(&index) {
        output[*len] = index;
        *len += 1;
    }
}

fn compact_panel_frame(screen: NSRect) -> NSRect {
    let width = (screen.size.width - PANEL_SCREEN_INSET_X * 2.0)
        .clamp(1.0, PANEL_MAX_WIDTH)
        .min(screen.size.width.max(1.0));
    let height = (screen.size.height - PANEL_SCREEN_INSET_Y * 2.0)
        .clamp(1.0, PANEL_MAX_HEIGHT)
        .min(screen.size.height.max(1.0));
    NSRect::new(
        NSPoint::new(
            screen.origin.x + (screen.size.width - width) * 0.5,
            screen.origin.y + (screen.size.height - height) * 0.5,
        ),
        NSSize::new(width, height),
    )
}

fn centered_rect(size: NSSize, container: NSRect) -> NSRect {
    let width = size.width.max(0.0).min(container.size.width.max(0.0));
    let height = size.height.max(0.0).min(container.size.height.max(0.0));
    NSRect::new(
        NSPoint::new(
            container.origin.x + (container.size.width - width) * 0.5,
            container.origin.y + (container.size.height - height) * 0.5,
        ),
        NSSize::new(width, height),
    )
}

fn aspect_fit_destination(source: NSSize, destination: NSRect) -> NSRect {
    if source.width <= 0.0
        || source.height <= 0.0
        || destination.size.width <= 0.0
        || destination.size.height <= 0.0
    {
        return centered_rect(NSSize::new(0.0, 0.0), destination);
    }
    let scale =
        (destination.size.width / source.width).min(destination.size.height / source.height);
    centered_rect(
        NSSize::new(source.width * scale, source.height * scale),
        destination,
    )
}

fn ellipsize(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_none() {
        prefix
    } else {
        format!("{prefix}…")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GridNavigation, app_strip_hit_rect, app_strip_start_x, aspect_fit_destination,
        bounds_intersection_area, centered_rect, compact_panel_frame, display_profile, ellipsize,
        fill_thumbnail_indexes, finder_badge_rect, grid_navigation_target, grid_viewport,
        least_recent_index, window_accessibility_label,
    };
    use crate::core::layout::GridMetrics;
    use crate::core::model::Bounds;
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    #[test]
    fn truncation_respects_unicode_scalar_boundaries() {
        assert_eq!(ellipsize("hello", 5), "hello");
        assert_eq!(ellipsize("窗口切换器", 3), "窗口切…");
    }

    #[test]
    fn grid_navigation_is_spatial_and_handles_partial_last_rows() {
        assert_eq!(
            grid_navigation_target(5, 10, 4, GridNavigation::Left),
            Some(4)
        );
        assert_eq!(
            grid_navigation_target(4, 10, 4, GridNavigation::Left),
            Some(4)
        );
        assert_eq!(
            grid_navigation_target(7, 10, 4, GridNavigation::Right),
            Some(7)
        );
        assert_eq!(
            grid_navigation_target(6, 10, 4, GridNavigation::Down),
            Some(9)
        );
        assert_eq!(
            grid_navigation_target(9, 10, 4, GridNavigation::Up),
            Some(5)
        );
        assert_eq!(
            grid_navigation_target(5, 10, 4, GridNavigation::Home),
            Some(0)
        );
        assert_eq!(
            grid_navigation_target(5, 10, 4, GridNavigation::End),
            Some(9)
        );
        assert_eq!(grid_navigation_target(0, 0, 4, GridNavigation::Down), None);
    }

    #[test]
    fn app_strip_hit_targets_are_centered_and_do_not_overlap() {
        let width = 1000.0;
        let count = 5;
        assert_eq!(app_strip_start_x(count, width), 395.0);
        let first = app_strip_hit_rect(0, count, width);
        let second = app_strip_hit_rect(1, count, width);
        assert_eq!(first.origin.x, 393.0);
        assert_eq!(first.size, NSSize::new(38.0, 38.0));
        assert!(first.origin.x + first.size.width < second.origin.x);
    }

    #[test]
    fn display_profile_uses_largest_overlap_and_marks_fullscreen_like_windows() {
        let displays = [
            Bounds::new(0.0, 0.0, 1920.0, 1080.0),
            Bounds::new(1920.0, 0.0, 1440.0, 900.0),
        ];
        let profile = display_profile(Bounds::new(1920.0, 0.0, 1440.0, 900.0), &displays);
        assert_eq!(profile.display_index, 1);
        assert!(profile.fullscreen_like);

        let crossing = Bounds::new(1800.0, 100.0, 500.0, 600.0);
        assert!(bounds_intersection_area(crossing, displays[1]) > 0.0);
        assert_eq!(display_profile(crossing, &displays).display_index, 1);
        assert!(!display_profile(crossing, &displays).fullscreen_like);
    }

    #[test]
    fn bounded_cache_eviction_selects_the_oldest_tick() {
        assert_eq!(least_recent_index([8, 3, 5]), Some(1));
        assert_eq!(least_recent_index([]), None);
    }

    #[test]
    fn accessibility_label_disambiguates_display_and_window_state() {
        assert_eq!(
            window_accessibility_label("Docs", "Arc", Some(2), true, false),
            "Docs, Arc, display 2, full screen"
        );
        assert_eq!(
            window_accessibility_label("", "Finder", None, false, true),
            "Finder, minimized"
        );
    }

    #[test]
    fn aspect_fit_centers_wide_and_tall_windows_without_cropping() {
        let preview = NSRect::new(NSPoint::new(20.0, 30.0), NSSize::new(280.0, 168.0));

        let wide = aspect_fit_destination(NSSize::new(1200.0, 300.0), preview);
        assert!((wide.origin.x - 20.0).abs() < 0.001);
        assert!((wide.size.width - 280.0).abs() < 0.001);
        assert!((wide.size.height - 70.0).abs() < 0.001);
        assert!((wide.origin.y - 79.0).abs() < 0.001);
        assert!(
            ((wide.origin.y - preview.origin.y)
                - (preview.origin.y + preview.size.height - wide.origin.y - wide.size.height))
                .abs()
                < 0.001
        );

        let tall = aspect_fit_destination(NSSize::new(420.0, 900.0), preview);
        assert!((tall.origin.y - 30.0).abs() < 0.001);
        assert!((tall.size.height - 168.0).abs() < 0.001);
        assert!(
            ((tall.origin.x - preview.origin.x)
                - (preview.origin.x + preview.size.width - tall.origin.x - tall.size.width))
                .abs()
                < 0.001
        );

        assert!((wide.size.width / wide.size.height - 4.0).abs() < 0.001);
        assert!((tall.size.width / tall.size.height - 420.0 / 900.0).abs() < 0.001);
    }

    #[test]
    fn preview_fallback_and_invalid_images_remain_centered() {
        let preview = NSRect::new(NSPoint::new(11.0, 17.0), NSSize::new(280.0, 168.0));
        let icon = centered_rect(NSSize::new(64.0, 64.0), preview);
        assert_eq!(icon.origin, NSPoint::new(119.0, 69.0));

        let invalid = aspect_fit_destination(NSSize::new(0.0, 80.0), preview);
        assert_eq!(invalid.size, NSSize::new(0.0, 0.0));
        assert_eq!(invalid.origin, NSPoint::new(151.0, 101.0));
    }

    #[test]
    fn finder_badge_is_small_and_inset_inside_the_preview() {
        let preview = NSRect::new(NSPoint::new(20.0, 30.0), NSSize::new(280.0, 168.0));
        let badge = finder_badge_rect(preview);
        assert_eq!(badge.origin, NSPoint::new(30.0, 40.0));
        assert_eq!(badge.size, NSSize::new(34.0, 34.0));
        assert!(badge.origin.x + badge.size.width < preview.origin.x + preview.size.width);
        assert!(badge.origin.y + badge.size.height < preview.origin.y + preview.size.height);
    }

    #[test]
    fn compact_panel_is_centered_and_bounded() {
        let screen = NSRect::new(NSPoint::new(-1920.0, 80.0), NSSize::new(1920.0, 1080.0));
        let panel = compact_panel_frame(screen);
        assert_eq!(panel.size, NSSize::new(1280.0, 760.0));
        assert_eq!(panel.origin, NSPoint::new(-1600.0, 240.0));
    }

    #[test]
    fn grid_viewport_excludes_header_and_bottom_inset() {
        let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1000.0, 700.0));
        let viewport = grid_viewport(bounds, GridMetrics::default());
        assert_eq!(viewport.origin.y, 112.0);
        assert_eq!(viewport.size.height, 548.0);
    }

    #[test]
    fn viewport_working_set_keeps_clipped_first_row_thumbnails() {
        let metrics = GridMetrics::default();
        let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1280.0, 760.0));
        let columns = metrics.columns(bounds.size.width);
        assert_eq!(columns, 4);
        let offset = metrics.reveal_offset(8, columns, bounds.size.height, 0.0, 20);
        assert_eq!(offset, 74.0);

        let mut indexes = [usize::MAX; 12];
        let len = fill_thumbnail_indexes(metrics, bounds, 20, 8, offset, |_| false, &mut indexes);
        let mut visible = indexes[..len].to_vec();
        visible.sort_unstable();

        assert_eq!(visible, (0..12).collect::<Vec<_>>());
        assert!(!visible.contains(&12));
    }

    #[test]
    fn newly_exposed_partial_row_is_prioritized_with_fixed_capacity() {
        let metrics = GridMetrics::default();
        let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1280.0, 760.0));
        let mut indexes = [usize::MAX; 12];
        let len = fill_thumbnail_indexes(
            metrics,
            bounds,
            20,
            8,
            100.0,
            |index| index < 12,
            &mut indexes,
        );

        assert_eq!(len, indexes.len());
        assert_eq!(indexes[0], 8);
        for newly_exposed in 12..16 {
            assert!(indexes.contains(&newly_exposed));
        }
    }

    #[test]
    fn partial_row_retention_is_bounded_to_sixteen_cards() {
        let metrics = GridMetrics::default();
        let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1280.0, 760.0));
        let viewport = grid_viewport(bounds, metrics);
        let item_count = 192;
        let columns = metrics.columns(bounds.size.width);
        let max_offset =
            (metrics.content_height(item_count, columns) - bounds.size.height).max(0.0) as usize;
        let mut peak_visible = 0;

        for offset in 0..=max_offset {
            let visible = (0..item_count)
                .filter(|index| {
                    let card = super::card_rect(
                        metrics,
                        *index,
                        item_count,
                        bounds.size.width,
                        offset as f64,
                    );
                    super::rects_intersect(card, viewport)
                })
                .count();
            peak_visible = peak_visible.max(visible);
            assert!(visible <= 16);
        }

        assert_eq!(peak_visible, 16);
    }
}
