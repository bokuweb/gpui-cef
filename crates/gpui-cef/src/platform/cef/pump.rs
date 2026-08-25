//! The timer that drives CEF's message loop.
//!
//! # Why not gpui's executor
//!
//! The first attempt ran the pump on `cx.spawn` plus
//! `background_executor().timer()`. When the app is idle, gpui's foreground
//! executor barely makes progress — measured at one tick every one to five
//! seconds. CEF only advances here, so at that rate a page never even finishes
//! loading.
//!
//! Worse, the dependency is circular: the pump does not run, so CEF produces no
//! frames, so gpui does not repaint, so the executor does not run, so the pump
//! does not run.
//!
//! So this bypasses gpui and installs a `CFRunLoopTimer` directly on the main
//! thread's run loop. It is registered in `kCFRunLoopCommonModes`, so it keeps
//! ticking while the run loop is nested — during a window resize, for instance.
//!
//! Because the callback comes straight from the run loop, nobody is borrowing
//! gpui's `App` at that moment. That matters: `do_message_loop_work()` runs
//! Chromium tasks, and gpui's paint callbacks can re-enter from inside them.

use std::{cell::RefCell, ffi::c_void, rc::Rc};

use core_foundation::{
    date::CFAbsoluteTimeGetCurrent,
    runloop::{kCFRunLoopCommonModes, CFRunLoop, CFRunLoopTimer, CFRunLoopTimerContext},
};
use gpui::{AsyncApp, WeakEntity};

use super::{shared::Shared, Webview};

/// How often to drive CEF, in seconds. Roughly 60fps.
const INTERVAL: f64 = 1. / 60.;

struct PumpState {
    shared: Rc<Shared>,
    /// On the shared-texture path, frame production has to be driven from here.
    accelerated: bool,
    webview: WeakEntity<Webview>,
    cx: RefCell<AsyncApp>,
}

/// Drives CEF for as long as it is alive; dropping it stops the timer.
pub(crate) struct Pump {
    timer: CFRunLoopTimer,
    // What the timer's context points at. Has to outlive the timer.
    _state: Box<PumpState>,
}

impl Pump {
    pub(crate) fn start(
        shared: Rc<Shared>,
        accelerated: bool,
        webview: WeakEntity<Webview>,
        cx: AsyncApp,
    ) -> Self {
        let state = Box::new(PumpState {
            shared,
            accelerated,
            webview,
            cx: RefCell::new(cx),
        });

        let mut context = CFRunLoopTimerContext {
            version: 0,
            info: &*state as *const PumpState as *mut c_void,
            retain: None,
            release: None,
            copyDescription: None,
        };

        let now = unsafe { CFAbsoluteTimeGetCurrent() };
        let timer = CFRunLoopTimer::new(now + INTERVAL, INTERVAL, 0, 0, on_tick, &mut context);

        unsafe {
            CFRunLoop::get_main().add_timer(&timer, kCFRunLoopCommonModes);
        }

        Self {
            timer,
            _state: state,
        }
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        unsafe {
            CFRunLoop::get_main().remove_timer(&self.timer, kCFRunLoopCommonModes);
        }
    }
}

extern "C" fn on_tick(_timer: core_foundation::runloop::CFRunLoopTimerRef, info: *mut c_void) {
    if info.is_null() {
        return;
    }
    let state = unsafe { &*(info as *const PumpState) };

    // Driving CEF here, with gpui's App unborrowed, is the whole point. Calling
    // this while the App is borrowed lets gpui's paint re-enter from the run
    // loop Chromium spins, which panics with "RefCell already borrowed".
    cef::do_message_loop_work();

    if state.shared.take_resized() {
        if let Some(host) = state.shared.host() {
            use cef::ImplBrowserHost as _;
            host.was_resized();
        }
    }

    if state.accelerated {
        // With external_begin_frame_enabled, this is what makes CEF draw a frame.
        if let Some(host) = state.shared.host() {
            use cef::ImplBrowserHost as _;
            host.send_external_begin_frame();
        }
    }

    let stale = state.shared.take_stale_cpu_frame();
    let dirty = state.shared.take_dirty();
    if stale.is_none() && !dirty {
        return;
    }

    let Ok(mut cx) = state.cx.try_borrow_mut() else {
        return;
    };
    let _ = state.webview.update(&mut *cx, |_, cx| {
        // Release the texture the CPU path is done with.
        if let Some(stale) = stale {
            cx.drop_image(stale, None);
        }
        if dirty {
            cx.notify();
        }
    });
}
