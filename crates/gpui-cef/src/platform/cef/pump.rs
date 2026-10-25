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
//! single timer per process and webviews register with it. CEF's scheduling
//! callback signals a thread-safe run-loop source to bring that timer forward;
//! idle/accelerated rendering retains the 60Hz fallback. A timer per webview
//! would pump CEF once per webview per tick, which is pure waste as soon as an
//! app shows more than one.

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    rc::Rc,
    sync::atomic::{AtomicPtr, Ordering},
    time::{Duration, Instant},
};

use core_foundation::{
    base::{kCFAllocatorDefault, TCFType},
    date::CFAbsoluteTimeGetCurrent,
    runloop::{
        kCFRunLoopCommonModes, CFRunLoop, CFRunLoopAddSource, CFRunLoopGetMain,
        CFRunLoopSourceContext, CFRunLoopSourceCreate, CFRunLoopSourceRef, CFRunLoopSourceSignal,
        CFRunLoopTimer, CFRunLoopTimerContext, CFRunLoopTimerSetNextFireDate, CFRunLoopWakeUp,
    },
};
use gpui::{AsyncApp, WeakEntity};

use super::{shared::Shared, Webview};

/// How often to drive CEF, in seconds. Roughly 60fps.
const INTERVAL: f64 = 1. / 60.;

#[derive(Default)]
struct PendingWork {
    deadline: Option<Instant>,
}

impl PendingWork {
    fn request(&mut self, now: Instant, delay_ms: i64) {
        // The fallback is 60Hz, so a longer request never needs a later wakeup.
        // Clamp before adding to Instant, including CEF's i64::MAX sentinel.
        self.deadline = Some(now + Duration::from_millis(delay_ms.clamp(0, 17) as u64));
    }

    fn take_deadline(&mut self, fallback: Instant) -> Option<Instant> {
        self.deadline.take().map(|deadline| deadline.min(fallback))
    }
}

static REQUESTED_WORK: parking_lot::Mutex<PendingWork> =
    parking_lot::Mutex::new(PendingWork { deadline: None });

// This source owns its create-rule retain for the process lifetime. Its context
// contains no GPUI or thread-local pointers. Signalling a CFRunLoopSource and
// waking its run loop are safe from CEF's arbitrary callback threads.
static WAKE_SOURCE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

pub(crate) fn schedule_work(delay_ms: i64) {
    REQUESTED_WORK.lock().request(Instant::now(), delay_ms);
    signal_wake_source();
}

fn signal_wake_source() {
    let source = WAKE_SOURCE.load(Ordering::Acquire);
    if !source.is_null() {
        unsafe {
            CFRunLoopSourceSignal(source as CFRunLoopSourceRef);
            CFRunLoopWakeUp(CFRunLoopGetMain());
        }
    }
}

// Only adjusts the main-thread timer. Never execute CEF inside the scheduling
// callback: a request can originate while application code has borrowed GPUI.
extern "C" fn on_schedule(_info: *const c_void) {
    let _ = PUMP.try_with(|slot| {
        let Ok(slot) = slot.try_borrow() else {
            signal_wake_source();
            return;
        };
        let Some(pump) = slot.as_ref() else {
            return;
        };
        let fallback = pump.state.last_tick.get() + Duration::from_secs_f64(INTERVAL);
        if let Some(deadline) = REQUESTED_WORK.lock().take_deadline(fallback) {
            let delay = deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64();
            unsafe {
                CFRunLoopTimerSetNextFireDate(
                    pump._timer.as_concrete_TypeRef(),
                    CFAbsoluteTimeGetCurrent() + delay,
                );
            }
        }
    });
}

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

struct PumpState {
    last_tick: Cell<Instant>,
    last_begin_frame: Cell<Instant>,
    pumping: Cell<bool>,
    deferred: Cell<bool>,
    next_id: Cell<u64>,
    views: RefCell<Vec<View>>,
}

impl PumpState {
    fn new(now: Instant) -> Self {
        Self {
            last_tick: Cell::new(now),
            last_begin_frame: Cell::new(now),
            pumping: Cell::new(false),
            deferred: Cell::new(false),
            next_id: Cell::new(0),
            views: RefCell::new(Vec::new()),
        }
    }

    fn begin_tick(&self) -> bool {
        if self.pumping.replace(true) {
            self.deferred.set(true);
            false
        } else {
            true
        }
    }

    fn finish_tick(&self) -> bool {
        self.pumping.set(false);
        self.deferred.replace(false)
    }

    fn take_begin_frame(&self, now: Instant) -> bool {
        if now
            .duration_since(self.last_begin_frame.get())
            .as_secs_f64()
            < INTERVAL
        {
            return false;
        }
        self.last_begin_frame.set(now);
        true
    }
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
        let state = Rc::new(PumpState::new(Instant::now()));

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

        let mut source_context = CFRunLoopSourceContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: on_schedule,
        };
        unsafe {
            let source = CFRunLoopSourceCreate(kCFAllocatorDefault, 0, &mut source_context);
            assert!(!source.is_null());
            CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopCommonModes);
            WAKE_SOURCE.store(source as *mut c_void, Ordering::Release);
        }
        // CEF can request work during initialization, before the first view.
        signal_wake_source();

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
    if !state.begin_tick() {
        return;
    }
    struct ActiveTick<'a>(&'a PumpState);
    impl Drop for ActiveTick<'_> {
        fn drop(&mut self) {
            if self.0.finish_tick() {
                schedule_work(0);
            }
        }
    }
    let _active = ActiveTick(state);
    state.last_tick.set(Instant::now());
    unsafe {
        CFRunLoopTimerSetNextFireDate(_timer, CFAbsoluteTimeGetCurrent() + INTERVAL);
    }

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

    // Urgent IPC/input work may pump much faster than the display cadence.
    // Do not turn those wakeups into extra accelerated frame requests.
    let begin_frame = state.take_begin_frame(Instant::now());
    for view in views {
        tick_view(&view, begin_frame);
    }
}

fn tick_view(view: &View, begin_frame: bool) {
    if view.shared.take_resized() {
        if let Some(host) = view.shared.host() {
            use cef::ImplBrowserHost as _;
            host.was_resized();
        }
    }

    if view.accelerated && begin_frame {
        // With external_begin_frame_enabled, this is what makes CEF draw a frame.
        if let Some(host) = view.shared.host() {
            use cef::ImplBrowserHost as _;
            host.send_external_begin_frame();
        }
    }

    let stale = view.shared.take_stale_cpu_frames();
    let events = view.shared.take_events();
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
        for event in events {
            cx.emit(event);
        }
        if dirty {
            cx.notify();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use cef::ImplBrowserProcessHandler;
    use proptest::prelude::*;

    #[test]
    fn cef_schedule_callback_accepts_requests_from_another_thread() {
        REQUESTED_WORK.lock().deadline = None;
        let before = Instant::now();
        std::thread::spawn(|| {
            super::super::handlers::GpuiCefBrowserProcessHandler::new()
                .on_schedule_message_pump_work(0);
        })
        .join()
        .unwrap();
        let deadline = REQUESTED_WORK
            .lock()
            .take_deadline(before + Duration::from_secs(1));
        assert!(
            deadline.is_some(),
            "CEF's immediate work request must not be ignored"
        );
        assert!(deadline.unwrap() <= Instant::now());
    }

    #[test]
    fn nested_ticks_defer_work_until_the_active_tick_finishes() {
        let state = PumpState::new(Instant::now());
        assert!(state.begin_tick());
        assert!(!state.begin_tick());
        assert!(!state.begin_tick());
        assert!(state.finish_tick());
        assert!(state.begin_tick());
        assert!(!state.finish_tick());
    }

    #[test]
    fn urgent_work_does_not_multiply_accelerated_frames() {
        let now = Instant::now();
        let state = PumpState::new(now);
        let mut frames = 0;
        for ms in 0..100 {
            frames += usize::from(state.take_begin_frame(now + Duration::from_millis(ms)));
        }
        assert_eq!(frames, 5);
        assert!(!state.take_begin_frame(now + Duration::from_millis(99)));
        assert!(state.take_begin_frame(now + Duration::from_millis(102)));
    }

    #[test]
    fn later_request_replaces_timer_but_cannot_postpone_fallback() {
        let now = Instant::now();
        let fallback = now + Duration::from_secs_f64(INTERVAL);
        let mut pending = PendingWork::default();
        pending.request(now, 1);
        pending.request(now, i64::MAX);
        assert_eq!(pending.take_deadline(fallback), Some(fallback));
        assert_eq!(pending.take_deadline(fallback), None);
        pending.request(now, i64::MIN);
        assert_eq!(pending.take_deadline(fallback), Some(now));
    }

    proptest! {
        #[test]
        fn burst_requests_follow_the_last_deadline(delays in prop::collection::vec(any::<i64>(), 1..256)) {
            let now = Instant::now();
            let fallback = now + Duration::from_secs_f64(INTERVAL);
            let mut pending = PendingWork::default();
            for delay in &delays { pending.request(now, *delay); }
            let expected = (now + Duration::from_millis((*delays.last().unwrap()).clamp(0, 17) as u64)).min(fallback);
            prop_assert_eq!(pending.take_deadline(fallback), Some(expected));
            prop_assert_eq!(pending.take_deadline(fallback), None);
        }
    }
}
