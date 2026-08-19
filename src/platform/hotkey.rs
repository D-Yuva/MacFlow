//! Low-overhead global keyboard event tap.
//!
//! The tap is installed on the main run loop and calls the supplied action
//! handler synchronously on the main thread. The handler must therefore stay
//! fast; no queueing or allocation is performed for ordinary events.

use std::cell::Cell;
use std::ffi::c_void;
use std::fmt;
use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::rc::Rc;

use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopMode, CFRunLoopSource, kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};

use crate::core::hotkey::{Action, HotkeyState, Input, Key, Modifiers, Shortcut};

const KEY_CODE_RETURN: i64 = 36;
const KEY_CODE_TAB: i64 = 48;
const KEY_CODE_SPACE: i64 = 49;
const KEY_CODE_ESCAPE: i64 = 53;
const KEY_CODE_KEYPAD_ENTER: i64 = 76;
const KEY_CODE_HOME: i64 = 115;
const KEY_CODE_END: i64 = 119;
const KEY_CODE_LEFT: i64 = 123;
const KEY_CODE_RIGHT: i64 = 124;
const KEY_CODE_DOWN: i64 = 125;
const KEY_CODE_UP: i64 = 126;

// Tap-disable notifications are delivered specially and must not be shifted
// into this mask (their event-type values are near u32::MAX).
const EVENT_MASK: u64 = (1_u64 << CGEventType::KeyDown.0) | (1_u64 << CGEventType::FlagsChanged.0);

/// Failure to install the global event tap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyTapError {
    /// Event taps are main-run-loop objects in MacFlow.
    NotMainThread,
    /// Core Foundation did not provide the process main run loop.
    MainRunLoopUnavailable,
    /// Core Foundation's common run-loop mode was unavailable.
    CommonRunLoopModeUnavailable,
    /// macOS rejected creation, usually because Accessibility access is off.
    EventTapUnavailable,
    /// The event tap was created, but its run-loop source was not.
    RunLoopSourceUnavailable,
}

impl fmt::Display for HotkeyTapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotMainThread => "the event tap must be installed on the main thread",
            Self::MainRunLoopUnavailable => "the main Core Foundation run loop is unavailable",
            Self::CommonRunLoopModeUnavailable => {
                "the Core Foundation common run-loop mode is unavailable"
            }
            Self::EventTapUnavailable => {
                "macOS rejected the event tap; Accessibility permission may be required"
            }
            Self::RunLoopSourceUnavailable => "could not create the event tap run-loop source",
        })
    }
}

impl std::error::Error for HotkeyTapError {}

struct CallbackContext {
    // Cell-based state keeps callback re-entry sound: application handoff code
    // is allowed to pump the run loop, so the callback must not hold `&mut`
    // across the handoff.
    state: Cell<HotkeyState>,
    shortcut: Cell<Shortcut>,
    handler: Box<dyn Fn(Action)>,
    // The port is retained by `HotkeyTap` for the full lifetime of this
    // context. A Cell avoids manufacturing a reference before tap creation.
    port: Cell<*const CFMachPort>,
    handler_panicked: Cell<bool>,
}

impl CallbackContext {
    fn new(shortcut: Shortcut, handler: Box<dyn Fn(Action)>) -> Self {
        Self {
            state: Cell::new(HotkeyState::default()),
            shortcut: Cell::new(shortcut),
            handler,
            port: Cell::new(ptr::null()),
            handler_panicked: Cell::new(false),
        }
    }

    fn reenable_tap(&self) {
        let port = self.port.get();
        if port.is_null() {
            return;
        }

        // SAFETY: `HotkeyTap` retains the port and clears this pointer only
        // after disabling/removing/invalidating it during Drop. Callbacks run
        // synchronously on that same main thread.
        CGEvent::tap_enable(unsafe { &*port }, true);
    }

    fn emit(&self, action: Action) -> bool {
        if self.handler_panicked.get() {
            return false;
        }

        // Never permit a user handler unwind to cross the system callback.
        // With the release profile's panic=abort, handlers must still avoid
        // panicking; this guard also makes debug/test builds FFI-safe.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.handler)(action);
        }));
        if result.is_err() {
            self.handler_panicked.set(true);
            self.state.set(HotkeyState::default());
            false
        } else {
            true
        }
    }
}

/// Owns a global keyboard event tap and its main-run-loop source.
///
/// Installation and destruction must happen on the main thread. The type is
/// deliberately neither `Send` nor `Sync`, preventing an installed tap from
/// being moved to a different thread.
pub struct HotkeyTap {
    run_loop: CFRetained<CFRunLoop>,
    common_mode: &'static CFRunLoopMode,
    source: CFRetained<CFRunLoopSource>,
    port: CFRetained<CFMachPort>,
    context: Rc<CallbackContext>,
    // One additional Rc strong count is owned by Core Graphics through this
    // raw user-info pointer. Drop releases it after invalidating the tap.
    context_user_info: NonNull<CallbackContext>,
    _main_thread_only: PhantomData<Rc<()>>,
}

impl HotkeyTap {
    /// Installs the configured switcher tap on the main run loop.
    ///
    /// `handler` is invoked synchronously on the main thread. It is boxed once
    /// during installation; the callback hot path does not allocate unless
    /// the handler itself does.
    pub fn install(
        shortcut: Shortcut,
        handler: impl Fn(Action) + 'static,
    ) -> Result<Self, HotkeyTapError> {
        // SAFETY: `pthread_main_np` has no preconditions and only queries the
        // identity of the calling thread.
        if unsafe { libc::pthread_main_np() } == 0 {
            return Err(HotkeyTapError::NotMainThread);
        }

        let run_loop = CFRunLoop::main().ok_or(HotkeyTapError::MainRunLoopUnavailable)?;
        // SAFETY: This reads the immutable Core Foundation process constant.
        let common_mode =
            unsafe { kCFRunLoopCommonModes }.ok_or(HotkeyTapError::CommonRunLoopModeUnavailable)?;

        let context = Rc::new(CallbackContext::new(shortcut, Box::new(handler)));
        let context_raw = Rc::into_raw(Rc::clone(&context)).cast_mut();
        let context_user_info =
            NonNull::new(context_raw).expect("Rc::into_raw always returns a non-null pointer");
        let context_ptr = context_raw.cast::<c_void>();

        // SAFETY: `context_ptr` owns an Rc strong count until after the tap is
        // disabled and invalidated in Drop. `event_tap_callback` uses exactly
        // the signature required by CGEventTapCreate.
        let port = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                EVENT_MASK,
                Some(event_tap_callback),
                context_ptr,
            )
        };
        let Some(port) = port else {
            // SAFETY: This balances the strong count just created with
            // `Rc::into_raw`; the tap was not created and cannot call back.
            unsafe { drop(Rc::from_raw(context_user_info.as_ptr())) };
            return Err(HotkeyTapError::EventTapUnavailable);
        };

        context.port.set(&*port as *const CFMachPort);

        let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
            CGEvent::tap_enable(&port, false);
            port.invalidate();
            context.port.set(ptr::null());
            // SAFETY: The invalidated port cannot call back, so the raw strong
            // count owned by its user-info slot can now be released.
            unsafe { drop(Rc::from_raw(context_user_info.as_ptr())) };
            return Err(HotkeyTapError::RunLoopSourceUnavailable);
        };

        run_loop.add_source(Some(&source), Some(common_mode));
        CGEvent::tap_enable(&port, true);

        Ok(Self {
            run_loop,
            common_mode,
            source,
            port,
            context,
            context_user_info,
            _main_thread_only: PhantomData,
        })
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.port.is_valid() && CGEvent::tap_is_enabled(&self.port)
    }

    /// Re-enables a valid tap, for explicit recovery or diagnostics.
    #[must_use]
    pub fn reenable(&self) -> bool {
        if !self.port.is_valid() {
            return false;
        }
        CGEvent::tap_enable(&self.port, true);
        CGEvent::tap_is_enabled(&self.port)
    }

    /// Reports whether the action handler panicked in an unwind-enabled build.
    /// A failed handler is quarantined, while the tap continues passing events.
    #[must_use]
    pub fn handler_panicked(&self) -> bool {
        self.context.handler_panicked.get()
    }

    /// Changes the trigger without reinstalling the event tap. Resetting the
    /// reducer prevents modifier state from the previous chord leaking into
    /// the newly selected shortcut.
    pub fn set_shortcut(&self, shortcut: Shortcut) {
        self.context.shortcut.set(shortcut);
        self.context.state.set(HotkeyState::default());
    }
}

impl Drop for HotkeyTap {
    fn drop(&mut self) {
        // Teardown order matters: stop delivery, detach from the run loop, then
        // invalidate both CF objects before releasing the callback context.
        CGEvent::tap_enable(&self.port, false);
        self.run_loop
            .remove_source(Some(&self.source), Some(self.common_mode));
        self.source.invalidate();
        self.port.invalidate();
        self.context.port.set(ptr::null());
        self.context.state.set(HotkeyState::default());
        // SAFETY: This balances the strong count transferred with
        // `Rc::into_raw` during installation. Invalidation above prevents any
        // new callbacks; a callback already on the stack owns its own count.
        unsafe { drop(Rc::from_raw(self.context_user_info.as_ptr())) };
    }
}

unsafe extern "C-unwind" fn event_tap_callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    let borrowed_event = event.as_ptr();
    if user_info.is_null() {
        return borrowed_event;
    }

    let context_ptr = user_info.cast::<CallbackContext>();
    // SAFETY: The tap owns a raw Rc strong count for this pointer until it is
    // invalidated. Taking a temporary count keeps the context alive even if
    // application code destroys `HotkeyTap` re-entrantly from its handler.
    unsafe { Rc::increment_strong_count(context_ptr) };
    // SAFETY: This consumes exactly the temporary count incremented above.
    let context = unsafe { Rc::from_raw(context_ptr) };

    // Keep even unforeseen Rust failures from unwinding into Core Graphics.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        callback_body(event_type, event, &context)
    }));
    result.unwrap_or(borrowed_event)
}

fn callback_body(
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    context: &CallbackContext,
) -> *mut CGEvent {
    let borrowed_event = event.as_ptr();

    let disabled = event_type == CGEventType::TapDisabledByTimeout
        || event_type == CGEventType::TapDisabledByUserInput;

    // Once an application handoff panics, fail open. The user must never lose
    // keyboard input because presentation code became unhealthy.
    if !disabled && context.handler_panicked.get() {
        return borrowed_event;
    }

    let input = if disabled {
        Input::TapDisabled
    } else {
        // SAFETY: Core Graphics guarantees the callback event is non-null and
        // valid for the duration of the callback. It remains borrowed here.
        let event_ref = unsafe { event.as_ref() };
        if event_type == CGEventType::FlagsChanged {
            Input::FlagsChanged(modifiers_from_flags(CGEvent::flags(Some(event_ref))))
        } else if event_type == CGEventType::KeyDown {
            let flags = CGEvent::flags(Some(event_ref));
            let key_code =
                CGEvent::integer_value_field(Some(event_ref), CGEventField::KeyboardEventKeycode);
            let is_repeat = CGEvent::integer_value_field(
                Some(event_ref),
                CGEventField::KeyboardEventAutorepeat,
            ) != 0;
            Input::KeyDown {
                key: key_from_code(key_code),
                modifiers: modifiers_from_flags(flags),
                is_repeat,
            }
        } else {
            return borrowed_event;
        }
    };

    let mut state = context.state.get();
    let outcome = state.handle_with_shortcut(input, context.shortcut.get());
    context.state.set(state);
    if disabled {
        // Re-enable before notifying application code, so a slow handoff cannot
        // leave the tap disabled.
        context.reenable_tap();
    }
    let handoff_succeeded = outcome.action.is_none_or(|action| context.emit(action));

    if outcome.consume && (disabled || handoff_succeeded) {
        ptr::null_mut()
    } else {
        // Pass-through is the exact incoming borrowed pointer. Returning a
        // retained event here would leak one retain for every observed event.
        borrowed_event
    }
}

fn modifiers_from_flags(flags: CGEventFlags) -> Modifiers {
    Modifiers {
        control: flags.contains(CGEventFlags::MaskControl),
        option: flags.contains(CGEventFlags::MaskAlternate),
        command: flags.contains(CGEventFlags::MaskCommand),
        shift: flags.contains(CGEventFlags::MaskShift),
    }
}

const fn key_from_code(key_code: i64) -> Key {
    match key_code {
        KEY_CODE_TAB => Key::Tab,
        KEY_CODE_SPACE => Key::Space,
        KEY_CODE_ESCAPE => Key::Escape,
        KEY_CODE_RETURN | KEY_CODE_KEYPAD_ENTER => Key::Return,
        KEY_CODE_LEFT => Key::Left,
        KEY_CODE_RIGHT => Key::Right,
        KEY_CODE_UP => Key::Up,
        KEY_CODE_DOWN => Key::Down,
        KEY_CODE_HOME => Key::Home,
        KEY_CODE_END => Key::End,
        _ => Key::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_only_switcher_control_keys() {
        assert_eq!(key_from_code(KEY_CODE_TAB), Key::Tab);
        assert_eq!(key_from_code(KEY_CODE_SPACE), Key::Space);
        assert_eq!(key_from_code(KEY_CODE_ESCAPE), Key::Escape);
        assert_eq!(key_from_code(KEY_CODE_RETURN), Key::Return);
        assert_eq!(key_from_code(KEY_CODE_KEYPAD_ENTER), Key::Return);
        assert_eq!(key_from_code(KEY_CODE_LEFT), Key::Left);
        assert_eq!(key_from_code(KEY_CODE_RIGHT), Key::Right);
        assert_eq!(key_from_code(KEY_CODE_UP), Key::Up);
        assert_eq!(key_from_code(KEY_CODE_DOWN), Key::Down);
        assert_eq!(key_from_code(KEY_CODE_HOME), Key::Home);
        assert_eq!(key_from_code(KEY_CODE_END), Key::End);
        assert_eq!(key_from_code(0), Key::Other);
    }

    #[test]
    fn maps_relevant_modifier_bits() {
        let flags =
            CGEventFlags::MaskControl | CGEventFlags::MaskAlternate | CGEventFlags::MaskShift;
        assert_eq!(
            modifiers_from_flags(flags),
            Modifiers {
                control: true,
                option: true,
                command: false,
                shift: true,
            }
        );
    }
}
