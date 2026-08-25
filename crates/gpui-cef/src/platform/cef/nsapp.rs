//! Makes gpui's `NSApplication` subclass usable by CEF.
//!
//! # Why this is needed
//!
//! There is exactly one `NSApp` per process, and it is instantiated as
//! **whichever class calls `sharedApplication` first**. Initializing CEF first
//! produces a plain `NSApplication`, and gpui then crashes when it fetches it
//! through `[GPUIApplication sharedApplication]` and finds none of its own ivars
//! (`Ivar platform not found on class NSKVONotifying_NSApplication`).
//!
//! So the order is reversed: gpui creates `NSApp`, then CEF is initialized. But
//! CEF requires `NSApp` to conform to `CefAppProtocol` — without it,
//! `cef_initialize` dies on a CHECK. gpui's class naturally knows nothing about
//! that, so it is retrofitted here through the Objective-C runtime:
//!
//! - add `isHandlingSendEvent` / `setHandlingSendEvent:` implementations
//! - swizzle `sendEvent:` to raise the flag around it
//! - declare conformance to the protocols CEF and Chromium check for
//!
//! `NSApp` only ever lives on the main thread, so a thread-local flag suffices.

use std::{cell::Cell, ffi::c_void};

use objc2::{
    ffi::{
        class_addMethod, class_addProtocol, class_conformsToProtocol, class_getInstanceMethod,
        method_getImplementation, method_setImplementation, objc_allocateProtocol, objc_getClass,
        objc_getProtocol, objc_registerProtocol, protocol_addMethodDescription, sel_registerName,
    },
    runtime::{AnyClass, AnyObject, Bool, Sel},
};

thread_local! {
    /// Whether `sendEvent:` is currently executing. Chromium uses this to detect
    /// nested event handling.
    static HANDLING_SEND_EVENT: Cell<bool> = const { Cell::new(false) };
}

/// The `sendEvent:` implementation that was in place before swizzling.
static mut ORIGINAL_SEND_EVENT: Option<
    unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject),
> = None;

extern "C-unwind" fn is_handling_send_event(_this: *mut AnyObject, _sel: Sel) -> Bool {
    Bool::new(HANDLING_SEND_EVENT.with(|flag| flag.get()))
}

extern "C-unwind" fn set_handling_send_event(_this: *mut AnyObject, _sel: Sel, handling: Bool) {
    HANDLING_SEND_EVENT.with(|flag| flag.set(handling.as_bool()));
}

extern "C-unwind" fn send_event(this: *mut AnyObject, sel: Sel, event: *mut AnyObject) {
    // Restore the previous value so nesting stays correct.
    let previous = HANDLING_SEND_EVENT.with(|flag| flag.replace(true));
    unsafe {
        #[allow(static_mut_refs)]
        if let Some(original) = ORIGINAL_SEND_EVENT {
            original(this, sel, event);
        }
    }
    HANDLING_SEND_EVENT.with(|flag| flag.set(previous));
}

/// The class gpui uses for `NSApp`. Has to match gpui's implementation.
const GPUI_APPLICATION_CLASS: &[u8] = b"GPUIApplication\0";

/// Retrofits the `CefAppProtocol` implementation onto `NSApp`'s class.
///
/// Run this exactly once, after gpui has created `NSApp` and before
/// `cef_initialize`.
pub(crate) fn adopt_cef_app_protocol() -> Result<(), &'static str> {
    unsafe {
        let class = objc_getClass(GPUI_APPLICATION_CLASS.as_ptr().cast());
        if class.is_null() {
            return Err("GPUIApplication class not found — call this after gpui created NSApp");
        }
        let class = class as *mut AnyClass;

        let sel = |name: &std::ffi::CStr| -> Result<Sel, &'static str> {
            sel_registerName(name.as_ptr()).ok_or("failed to register selector")
        };

        // An objc IMP hides its arguments, so the real functions are transmuted.
        let as_imp = |f: *const c_void| -> unsafe extern "C-unwind" fn() {
            std::mem::transmute::<*const c_void, unsafe extern "C-unwind" fn()>(f)
        };

        if !class_addMethod(
            class,
            sel(c"isHandlingSendEvent")?,
            as_imp(is_handling_send_event as *const c_void),
            c"c@:".as_ptr(),
        )
        .as_bool()
        {
            return Err("failed to add -isHandlingSendEvent");
        }

        if !class_addMethod(
            class,
            sel(c"setHandlingSendEvent:")?,
            as_imp(set_handling_send_event as *const c_void),
            c"v@:c".as_ptr(),
        )
        .as_bool()
        {
            return Err("failed to add -setHandlingSendEvent:");
        }

        // NSApplication already implements sendEvent:, so wrap it instead.
        let send_event_sel = sel(c"sendEvent:")?;
        let method = class_getInstanceMethod(class, send_event_sel);
        if method.is_null() {
            return Err("-sendEvent: not found on GPUIApplication");
        }
        ORIGINAL_SEND_EVENT = method_getImplementation(method).map(|imp| {
            std::mem::transmute::<
                unsafe extern "C-unwind" fn(),
                unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject),
            >(imp)
        });
        method_setImplementation(method, as_imp(send_event as *const c_void));

        // Both CEF and Chromium gate on conformsToProtocol: and abort with a
        // CHECK otherwise. The protocols themselves only exist in headers and are
        // never registered with the Objective-C runtime, so register them here
        // under the same names — conformsToProtocol: compares by name.
        for name in [c"CrAppProtocol", c"CrAppControlProtocol", c"CefAppProtocol"] {
            let mut protocol = objc_getProtocol(name.as_ptr());
            if protocol.is_null() {
                let allocated = objc_allocateProtocol(name.as_ptr());
                if allocated.is_null() {
                    return Err("failed to allocate the protocol");
                }
                protocol_addMethodDescription(
                    allocated,
                    sel(c"isHandlingSendEvent")?,
                    c"c@:".as_ptr(),
                    Bool::YES,
                    Bool::YES,
                );
                protocol_addMethodDescription(
                    allocated,
                    sel(c"setHandlingSendEvent:")?,
                    c"v@:c".as_ptr(),
                    Bool::YES,
                    Bool::YES,
                );
                objc_registerProtocol(allocated);
                protocol = allocated;
            }
            class_addProtocol(class, protocol);

            if !class_conformsToProtocol(class, protocol).as_bool() {
                return Err("NSApp class does not conform to the protocol CEF requires");
            }
        }

        Ok(())
    }
}
