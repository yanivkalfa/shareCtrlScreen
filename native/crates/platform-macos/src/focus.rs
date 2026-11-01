//! `InputInjector::focus_window_at` on macOS: bring the window under a point
//! to the front **without clicking it**, so the paste chord that follows a
//! file drop lands in the window the file was dropped on (a synthetic click
//! could press whatever button sits under the drop point).
//!
//! Three steps, each for a reason:
//!
//! 1. **Which app.** `CGWindowListCopyWindowInfo` lists the on-screen
//!    windows front to back; the first normal-layer window containing the
//!    point names the app (`inject_state::window_at`). This works for every
//!    app, accessible or not, and needs no Screen Recording grant: without it
//!    macOS withholds only window *names*, not bounds, layer or owner.
//! 2. **Which window.** Accessibility hit-tests inside that app
//!    (`AXUIElementCopyElementAtPosition`) and raises the window holding the
//!    element (`AXRaise`, `AXMain`). Activating an app alone brings forward
//!    its last key window, which for an app with several windows is not
//!    necessarily the one under the point.
//! 3. **Activate the app,** twice over: `AXFrontmost` (performed by the target
//!    app itself, so not subject to macOS 14's cooperative-activation rules,
//!    but only for apps with accessibility support) and
//!    `NSRunningApplication.activate(options:)` (any app, but documented as
//!    not guaranteed, and since macOS 14 a request from an app that is not
//!    active itself may be refused). Either succeeding is a success.
//!
//! ## References
//! * `CGWindowListCopyWindowInfo`, required keys (`kCGWindowLayer`,
//!   `kCGWindowBounds`, `kCGWindowAlpha`, `kCGWindowOwnerPID`):
//!   <https://developer.apple.com/documentation/coregraphics/cgwindowlistcopywindowinfo(_:_:)>
//!   and <https://developer.apple.com/documentation/coregraphics/required-window-list-keys>
//! * `CGRectMakeWithDictionaryRepresentation`:
//!   <https://developer.apple.com/documentation/corefoundation/cgrect/init(dictionaryrepresentation:)>
//! * `AXUIElementCreateApplication`, `AXUIElementCopyElementAtPosition`,
//!   `AXUIElementCopyAttributeValue`, `AXUIElementSetAttributeValue`,
//!   `AXUIElementPerformAction`, `AXUIElementSetMessagingTimeout`:
//!   <https://developer.apple.com/documentation/applicationservices/axuielement_h>
//! * `kAXRaiseAction`, `kAXWindowAttribute`, `kAXRoleAttribute`,
//!   `kAXMainAttribute`, `kAXFrontmostAttribute` (`CFSTR` macros in
//!   `AXActionConstants.h` / `AXAttributeConstants.h`):
//!   <https://developer.apple.com/documentation/applicationservices/carbon_accessibility/attributes>
//! * `NSRunningApplication.activate(options:)` ("the framework also does not
//!   guarantee that the app will be activated at all"):
//!   <https://developer.apple.com/documentation/appkit/nsrunningapplication/activate(options:)>
//! * macOS 14 cooperative app activation (WWDC23 "What's new in AppKit"):
//!   <https://developer.apple.com/videos/play/wwdc2023/10054/>

use std::ptr::{self, NonNull};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::{
    kCFBooleanTrue, CFArray, CFDictionary, CFRetained, CFString, CFType, CGRect,
};
use objc2_core_graphics::{
    kCGNullWindowID, kCGWindowAlpha, kCGWindowBounds, kCGWindowLayer, kCGWindowOwnerPID,
    CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo, CGWindowListOption,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

use crate::displays;
use crate::inject_state::{window_at, WindowInfo};

/// Bound on each Accessibility call. They are synchronous IPC into the
/// target app and run on the host transport thread; without a bound a hung
/// app would stall the session for AX's default of about 6 s.
const AX_TIMEOUT_S: f32 = 0.5;

// The AX names are `CFSTR` macros in the SDK headers, so there is no symbol
// to link against; these are their documented values.
const AX_RAISE_ACTION: &str = "AXRaise";
/// Both `kAXWindowAttribute` (an element's window) and `kAXWindowRole`.
const AX_WINDOW: &str = "AXWindow";
const AX_ROLE_ATTRIBUTE: &str = "AXRole";
const AX_MAIN_ATTRIBUTE: &str = "AXMain";
const AX_FRONTMOST_ATTRIBUTE: &str = "AXFrontmost";

/// Brings the window under the global point `(x, y)` (points) forward.
/// `false` when there is no window there or macOS refused every route.
pub(crate) fn focus_window_at(x: f64, y: f64) -> bool {
    let Some(pid) = window_at(on_screen_windows(), x, y) else {
        return false;
    };
    let frontmost = raise_window_at(pid, x, y);
    let activated = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .is_some_and(|app| app.activateWithOptions(NSApplicationActivationOptions::empty()));
    if !(frontmost || activated) {
        tracing::info!(
            pid,
            "macOS refused to bring the window under the drop point forward"
        );
    }
    frontmost || activated
}

/// The on-screen windows, front to back.
fn on_screen_windows() -> Vec<WindowInfo> {
    let options =
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
    let Some(list) = CGWindowListCopyWindowInfo(options, kCGNullWindowID) else {
        tracing::warn!("the window server returned no window list");
        return Vec::new();
    };
    // SAFETY: CFArray is toll-free bridged to NSArray; the function documents
    // its elements as CFDictionaries keyed by CFString constants, which are
    // toll-free bridged to NSDictionary<NSString, id>.
    let array: &NSArray<NSDictionary<NSString, AnyObject>> = unsafe {
        &*(&*list as *const CFArray).cast::<NSArray<NSDictionary<NSString, AnyObject>>>()
    };
    array.iter().filter_map(|d| window_info(&d)).collect()
}

/// A CoreFoundation string key as the NSString it is toll-free bridged to.
fn ns_key(key: &CFString) -> &NSString {
    // SAFETY: CFString and NSString are toll-free bridged.
    unsafe { &*(key as *const CFString).cast::<NSString>() }
}

/// The fields of one window entry; `None` if a required key is missing or
/// malformed (the entry is then skipped, never guessed).
fn window_info(d: &NSDictionary<NSString, AnyObject>) -> Option<WindowInfo> {
    // SAFETY: framework constants, valid immortal CFStrings.
    let (pid_key, layer_key, alpha_key, bounds_key) = unsafe {
        (
            kCGWindowOwnerPID,
            kCGWindowLayer,
            kCGWindowAlpha,
            kCGWindowBounds,
        )
    };
    let number = |key: &CFString| {
        d.objectForKey(ns_key(key))
            .and_then(|v| v.downcast::<NSNumber>().ok())
    };
    let bounds = d
        .objectForKey(ns_key(bounds_key))?
        .downcast::<NSDictionary>()
        .ok()?;
    let mut rect = CGRect::default();
    // SAFETY: NSDictionary is toll-free bridged to CFDictionary; `rect` is a
    // valid out pointer. Returns false if the dictionary is not a CGRect.
    let ok = unsafe {
        CGRectMakeWithDictionaryRepresentation(
            Some(&*Retained::as_ptr(&bounds).cast::<CFDictionary>()),
            &mut rect,
        )
    };
    if !ok {
        return None;
    }
    Some(WindowInfo {
        pid: number(pid_key)?.intValue(),
        layer: number(layer_key)?.integerValue() as i64,
        alpha: number(alpha_key)?.doubleValue(),
        bounds: displays::rect_f(rect),
    })
}

/// Steps 2 and 3a of the module docs. Returns whether the app accepted
/// `AXFrontmost`; raising the window is best effort on top of that.
fn raise_window_at(pid: i32, x: f64, y: f64) -> bool {
    // SAFETY: any pid is accepted; for a process without an accessibility
    // server the calls below fail with an error code.
    let app = unsafe { AXUIElement::new_application(pid) };
    set_timeout(&app);
    match window_element_at(&app, x, y) {
        Some(window) => {
            set_timeout(&window);
            // SAFETY: a valid element and action name.
            let raised =
                unsafe { window.perform_action(&CFString::from_static_str(AX_RAISE_ACTION)) };
            if raised != AXError::Success {
                tracing::debug!(pid, err = raised.0, "AXRaise refused");
            }
            set_true(&window, AX_MAIN_ATTRIBUTE);
        }
        None => tracing::debug!(pid, "no accessible window under the point"),
    }
    set_true(&app, AX_FRONTMOST_ATTRIBUTE)
}

fn set_timeout(element: &AXUIElement) {
    // SAFETY: a valid element and a positive timeout.
    let err = unsafe { element.set_messaging_timeout(AX_TIMEOUT_S) };
    if err != AXError::Success {
        tracing::debug!(err = err.0, "AXUIElementSetMessagingTimeout failed");
    }
}

/// The window holding the element of `app` at the point: the element itself
/// when it is a window, else its `AXWindow`.
fn window_element_at(app: &AXUIElement, x: f64, y: f64) -> Option<CFRetained<AXUIElement>> {
    let mut hit: *const AXUIElement = ptr::null();
    // SAFETY: `hit` is a valid out pointer. Global coordinates in points fit
    // an f32 exactly (far below 2^24).
    let err = unsafe { app.copy_element_at_position(x as f32, y as f32, NonNull::from(&mut hit)) };
    if err != AXError::Success {
        return None;
    }
    // SAFETY: on success the out pointer holds a +1 reference (Copy rule).
    let hit = unsafe { CFRetained::from_raw(NonNull::new(hit.cast_mut())?) };
    let is_window = attribute(&hit, AX_ROLE_ATTRIBUTE)
        .and_then(|role| role.downcast::<CFString>().ok())
        .is_some_and(|role| role.to_string() == AX_WINDOW);
    if is_window {
        return Some(hit);
    }
    attribute(&hit, AX_WINDOW)?.downcast::<AXUIElement>().ok()
}

fn attribute(element: &AXUIElement, name: &'static str) -> Option<CFRetained<CFType>> {
    let mut value: *const CFType = ptr::null();
    // SAFETY: `value` is a valid out pointer.
    let err = unsafe {
        element.copy_attribute_value(&CFString::from_static_str(name), NonNull::from(&mut value))
    };
    if err != AXError::Success {
        return None;
    }
    // SAFETY: on success the out pointer holds a +1 reference (Copy rule).
    NonNull::new(value.cast_mut()).map(|v| unsafe { CFRetained::from_raw(v) })
}

/// Sets a boolean attribute to true; whether the app accepted it.
fn set_true(element: &AXUIElement, name: &'static str) -> bool {
    // SAFETY: framework constant; only absent if CoreFoundation failed to load.
    let Some(yes) = (unsafe { kCFBooleanTrue }) else {
        return false;
    };
    // SAFETY: the attributes set here (`AXMain`, `AXFrontmost`) take a
    // CFBoolean, which is what `yes` is.
    let err = unsafe { element.set_attribute_value(&CFString::from_static_str(name), yes) };
    err == AXError::Success
}
