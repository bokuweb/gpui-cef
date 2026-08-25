//! State shared between CEF's callbacks and gpui's rendering.
//!
//! CEF runs with `external_message_pump` and a single-threaded message loop, so
//! `RenderHandler` and friends are called on the same main thread as gpui.
//! `Rc<RefCell<_>>` is therefore enough — and in fact preferable, since
//! `CVPixelBuffer` is not `Send`.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use cef::{Browser, BrowserHost, ImplBrowser};
use gpui::{px, size, Bounds, Pixels, RenderImage, Size};

/// The most recent frame handed over by CEF.
pub(crate) enum Frame {
    /// From `on_accelerated_paint`: a GPU texture referenced in place.
    Accelerated(core_video::pixel_buffer::CVPixelBuffer),
    /// From `on_paint`: BGRA pixels copied on the CPU.
    Cpu(Arc<RenderImage>),
}

/// Shared state for one webview.
pub(crate) struct Shared {
    /// The rect gpui laid this out at, in window space. Used to translate mouse
    /// positions into view space.
    bounds: Cell<Bounds<Pixels>>,
    /// The logical size (DIP) reported by `RenderHandler::get_view_rect`.
    view_size: Cell<Size<Pixels>>,
    /// The scale factor reported by `RenderHandler::get_screen_info`.
    scale_factor: Cell<f32>,
    /// The latest frame.
    frame: RefCell<Option<Frame>>,
    /// The previous CPU frame handed to gpui, kept so it can be dropped from
    /// gpui's texture cache.
    stale_cpu_frame: RefCell<Option<Arc<RenderImage>>>,
    /// Set when a new frame arrives; the pump picks this up and calls `notify()`.
    dirty: Cell<bool>,
    /// Set when `view_size` changed; the pump picks this up and calls
    /// `was_resized()`.
    resized: Cell<bool>,
    /// Page title and URL.
    title: RefCell<String>,
    url: RefCell<String>,
    /// Whether a page is loading, as reported by CEF's `LoadHandler`.
    is_loading: Cell<bool>,
    /// Whether history navigation is possible.
    can_go_back: Cell<bool>,
    can_go_forward: Cell<bool>,
    /// Whether the first frame has arrived, purely so the log can confirm the
    /// pipeline is alive.
    received_frame: Cell<bool>,
    /// The browser, kept here so the message pump can reach `BrowserHost`
    /// without borrowing gpui's `App`.
    browser: RefCell<Option<Browser>>,
}

impl Shared {
    pub(crate) fn new(url: String) -> Rc<Self> {
        Rc::new(Self {
            bounds: Cell::new(Bounds::default()),
            // CEF rejects a zero-sized view, so start out non-zero.
            view_size: Cell::new(size(px(800.), px(600.))),
            scale_factor: Cell::new(1.),
            frame: RefCell::new(None),
            stale_cpu_frame: RefCell::new(None),
            dirty: Cell::new(false),
            resized: Cell::new(false),
            title: RefCell::new(String::new()),
            url: RefCell::new(url),
            is_loading: Cell::new(false),
            can_go_back: Cell::new(false),
            can_go_forward: Cell::new(false),
            received_frame: Cell::new(false),
            browser: RefCell::new(None),
        })
    }

    /// Called from gpui's prepaint. Raises `resized` when the size changed.
    pub(crate) fn set_layout(&self, bounds: Bounds<Pixels>, scale_factor: f32) {
        self.bounds.set(bounds);

        let new_size = size(
            px(f32::from(bounds.size.width).max(1.)),
            px(f32::from(bounds.size.height).max(1.)),
        );
        let old_size = self.view_size.get();
        // Ignore sub-pixel differences, so was_resized() is not called every frame.
        if (f32::from(old_size.width) - f32::from(new_size.width)).abs() >= 1.
            || (f32::from(old_size.height) - f32::from(new_size.height)).abs() >= 1.
            || (self.scale_factor.get() - scale_factor).abs() >= f32::EPSILON
        {
            self.view_size.set(new_size);
            self.scale_factor.set(scale_factor);
            self.resized.set(true);
        }
    }

    pub(crate) fn set_browser(&self, browser: Browser) {
        *self.browser.borrow_mut() = Some(browser);
    }

    pub(crate) fn host(&self) -> Option<BrowserHost> {
        self.browser
            .borrow()
            .as_ref()
            .and_then(|browser| browser.host())
    }

    pub(crate) fn bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }

    pub(crate) fn view_size(&self) -> Size<Pixels> {
        self.view_size.get()
    }

    pub(crate) fn scale_factor(&self) -> f32 {
        self.scale_factor.get()
    }

    pub(crate) fn put_frame(&self, frame: Frame) {
        // One line in the log is enough to tell which path is live.
        if !self.received_frame.replace(true) {
            match &frame {
                Frame::Accelerated(buffer) => log::info!(
                    "first frame via shared IOSurface: {}x{}",
                    buffer.get_width(),
                    buffer.get_height()
                ),
                Frame::Cpu(image) => {
                    let size = image.size(0);
                    log::info!(
                        "first frame via CPU copy: {}x{} (accelerated path unavailable)",
                        size.width.0,
                        size.height.0
                    )
                }
            }
        }

        // If the outgoing frame was a CPU one, it has to be dropped from gpui's
        // texture cache, so remember it.
        if let Some(Frame::Cpu(old)) = self.frame.replace(Some(frame)) {
            *self.stale_cpu_frame.borrow_mut() = Some(old);
        }
        self.dirty.set(true);
    }

    pub(crate) fn with_frame<R>(&self, f: impl FnOnce(Option<&Frame>) -> R) -> R {
        f(self.frame.borrow().as_ref())
    }

    /// Takes the stale frame that gpui should release from its texture cache.
    pub(crate) fn take_stale_cpu_frame(&self) -> Option<Arc<RenderImage>> {
        self.stale_cpu_frame.borrow_mut().take()
    }

    pub(crate) fn take_dirty(&self) -> bool {
        self.dirty.replace(false)
    }

    pub(crate) fn take_resized(&self) -> bool {
        self.resized.replace(false)
    }

    pub(crate) fn set_title(&self, title: String) {
        *self.title.borrow_mut() = title;
        self.dirty.set(true);
    }

    pub(crate) fn set_url(&self, url: String) {
        *self.url.borrow_mut() = url;
        self.dirty.set(true);
    }

    pub(crate) fn set_load_state(&self, is_loading: bool, can_go_back: bool, can_go_forward: bool) {
        self.is_loading.set(is_loading);
        self.can_go_back.set(can_go_back);
        self.can_go_forward.set(can_go_forward);
        self.dirty.set(true);
    }

    pub(crate) fn is_loading(&self) -> bool {
        self.is_loading.get()
    }

    pub(crate) fn can_go_back(&self) -> bool {
        self.can_go_back.get()
    }

    pub(crate) fn can_go_forward(&self) -> bool {
        self.can_go_forward.get()
    }

    pub(crate) fn title(&self) -> String {
        self.title.borrow().clone()
    }

    pub(crate) fn url(&self) -> String {
        self.url.borrow().clone()
    }
}
