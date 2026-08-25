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
//!
//! # One timer for the whole process
//!
//! `do_message_loop_work()` drives all of CEF, not one browser, so there is a
//! single timer per process and webviews register with it. A timer per webview
//! would pump CEF once per webview per tick, which is pure waste as soon as an
//! app shows more than one.

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    rc::Rc,
};

use core_foundation::{
    date::CFAbsoluteTimeGetCurrent,
    runloop::{kCFRunLoopCommonModes, CFRunLoop, CFRunLoopTimer, CFRunLoopTimerContext},
};
use gpui::{AsyncApp, WeakEntity};

use super::{shared::Shared, Webview};

/// How often to drive CEF, in seconds. Roughly 60fps.
const INTERVAL: f64 = 1. / 60.;

thread_local! {
    /// The one timer, created on first use and left running for the life of the
    /// process. CEF still needs pumping for its own housekeeping after the last
    /// webview goes away.
    static PUMP: RefCell<Option<Pump>> = const { RefCell::new(None) };
}

#[derive(Clone)]
struct View {
    id: u64,
    shared: Rc<Shared>,
    /// On the shared-texture path, frame production has to be driven from here.
    accelerated: bool,
    webview: WeakEntity<Webview>,
    cx: AsyncApp,
}

#[derive(Default)]
struct PumpState {
    next_id: Cell<u64>,
    views: RefCell<Vec<View>>,
}

struct Pump {
    // The timer is never removed, but keep it so its lifetime is explicit.
    _timer: CFRunLoopTimer,
    // What the timer's context points at. Has to outlive the timer.
    state: Rc<PumpState>,
}

/// Keeps one webview registered with the pump; unregisters on drop.
pub(crate) struct Registration {
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let id = self.id;
        PUMP.with(|slot| {
            // The pump never borrows this slot from inside the tick, but stay
            // defensive: failing to unregister is better than a panic on drop.
            if let Ok(slot) = slot.try_borrow() {
                if let Some(pump) = slot.as_ref() {
                    if let Ok(mut views) = pump.state.views.try_borrow_mut() {
                        views.retain(|view| view.id != id);
                    }
                }
            }
        });
    }
}

/// Registers a webview with the process-wide pump, starting it if needed.
pub(crate) fn register(
    shared: Rc<Shared>,
    accelerated: bool,
    webview: WeakEntity<Webview>,
    cx: AsyncApp,
) -> Registration {
    PUMP.with(|slot| {
        let mut slot = slot.borrow_mut();
        let pump = slot.get_or_insert_with(Pump::start);

        let id = pump.state.next_id.get();
        pump.state.next_id.set(id + 1);
        pump.state.views.borrow_mut().push(View {
            id,
            shared,
            accelerated,
            webview,
            cx,
        });

        Registration { id }
    })
}

impl Pump {
    fn start() -> Self {
        let state = Rc::new(PumpState::default());

        let mut context = CFRunLoopTimerContext {
            version: 0,
            info: Rc::as_ptr(&state) as *mut c_void,
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
            _timer: timer,
            state,
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

    // Snapshot, because the updates below run application code that may add or
    // remove webviews.
    let views = match state.views.try_borrow() {
        Ok(views) => views.clone(),
        Err(_) => return,
    };

    for view in views {
        tick_view(&view);
    }
}

fn tick_view(view: &View) {
    if view.shared.take_resized() {
        if let Some(host) = view.shared.host() {
            use cef::ImplBrowserHost as _;
            host.was_resized();
        }
    }

    if view.accelerated {
        // With external_begin_frame_enabled, this is what makes CEF draw a frame.
        if let Some(host) = view.shared.host() {
            use cef::ImplBrowserHost as _;
            host.send_external_begin_frame();
        }
    }

    let stale = view.shared.take_stale_cpu_frames();
    let dirty = view.shared.take_dirty();
    if stale.is_empty() && !dirty {
        return;
    }

    let mut cx = view.cx.clone();
    let _ = view.webview.update(&mut cx, |_, cx| {
        // Release the textures the CPU path is done with.
        for frame in stale {
            cx.drop_image(frame, None);
        }
        if dirty {
            cx.notify();
        }
    });
}
