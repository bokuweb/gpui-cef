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

use cef::{
    BeforeDownloadCallback, Browser, BrowserHost, CefString, CefStringList, DownloadItemCallback,
    FileDialogCallback, ImplBeforeDownloadCallback, ImplBrowser, ImplDownloadItemCallback,
    ImplFileDialogCallback, ImplJsdialogCallback, JsdialogCallback,
};
use gpui::{px, size, Bounds, CursorStyle, Pixels, RenderImage, Size};

use super::pending::{Abort, Cancel, Downloads, Pending, Proceed};
use crate::{DialogId, DownloadId, DownloadState, FileDialogId, WebviewEvent};

/// The most recent frame handed over by CEF.
pub(crate) enum Frame {
    /// From `on_accelerated_paint`: a GPU texture referenced in place.
    Accelerated(core_video::pixel_buffer::CVPixelBuffer),
    /// From `on_paint`: BGRA pixels copied on the CPU.
    Cpu(Arc<RenderImage>),
}

/// One layer's worth of frame bookkeeping. CEF paints the page and any popup
/// (a `<select>` dropdown, say) as separate layers, and each needs the same
/// "latest frame plus the texture gpui should now release" handling.
#[derive(Default)]
struct FrameSlot {
    frame: RefCell<Option<Frame>>,
    stale_cpu: RefCell<Option<Arc<RenderImage>>>,
}

impl FrameSlot {
    fn put(&self, frame: Frame) {
        // If the outgoing frame was a CPU one, it has to be dropped from gpui's
        // texture cache, so remember it.
        if let Some(Frame::Cpu(old)) = self.frame.replace(Some(frame)) {
            *self.stale_cpu.borrow_mut() = Some(old);
        }
    }

    fn clear(&self) {
        if let Some(Frame::Cpu(old)) = self.frame.replace(None) {
            *self.stale_cpu.borrow_mut() = Some(old);
        }
    }

    fn take_stale(&self) -> Option<Arc<RenderImage>> {
        self.stale_cpu.borrow_mut().take()
    }
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
    /// The latest frame of the page itself.
    view: FrameSlot,
    /// The latest frame of the popup layer, and where to draw it.
    popup: FrameSlot,
    popup_visible: Cell<bool>,
    popup_rect: Cell<Bounds<Pixels>>,
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
    /// The cursor the page wants, as reported by CEF's `OnCursorChange`.
    cursor: Cell<CursorStyle>,
    /// The text the OS IME is currently composing. The page owns the real text;
    /// this mirror exists only so the IME has something coherent to ask about.
    composition: RefCell<String>,
    /// Where CEF says the composition is on screen, in the webview's own
    /// coordinate space. Used to place the candidate window.
    composition_bounds: RefCell<Vec<Bounds<Pixels>>>,
    /// Whether the first frame has arrived, purely so the log can confirm the
    /// pipeline is alive.
    received_frame: Cell<bool>,
    /// The browser, kept here so the message pump can reach `BrowserHost`
    /// without borrowing gpui's `App`.
    browser: RefCell<Option<Browser>>,
    /// Events waiting for the pump to emit them. CEF's callbacks run inside
    /// `do_message_loop_work()`, where gpui cannot be reached, so they queue
    /// here instead.
    events: RefCell<Vec<WebviewEvent>>,
    /// JavaScript dialogs waiting for `Webview::resolve_dialog`.
    js_dialogs: Pending<JsDialogReply>,
    /// File choosers waiting for `Webview::resolve_file_dialog`.
    file_dialogs: Pending<FileDialogReply>,
    /// Downloads, from the question of where they go until they finish.
    downloads: Downloads<BeforeDownloadCallback, DownloadItemCallback>,
    /// Set once the webview is gone. Anything the page asks for after that is
    /// turned down on the spot, since nobody is left to answer.
    closed: Cell<bool>,
}

/// The answer a JavaScript dialog is waiting for.
pub(crate) struct JsDialogReply(pub(crate) JsdialogCallback);

impl JsDialogReply {
    fn answer(self, accept: bool, text: Option<&str>) {
        let text = text.map(CefString::from);
        self.0.cont(accept.into(), text.as_ref());
    }
}

impl Cancel for JsDialogReply {
    fn cancel(self) {
        self.answer(false, None);
    }
}

/// The choice a file chooser is waiting for.
pub(crate) struct FileDialogReply(pub(crate) FileDialogCallback);

impl FileDialogReply {
    fn answer(self, paths: &[std::path::PathBuf]) {
        // CEF reads an empty list as a cancellation.
        let mut list = CefStringList::new();
        for path in paths {
            list.append(&path.to_string_lossy());
        }
        self.0.cont(Some(&mut list));
    }
}

impl Cancel for FileDialogReply {
    fn cancel(self) {
        self.0.cancel();
    }
}

impl Proceed for BeforeDownloadCallback {
    fn proceed(self, path: &std::path::Path) {
        let path = CefString::from(path.to_string_lossy().as_ref());
        // No "Save As" dialog: the application already chose.
        self.cont(Some(&path), 0);
    }
}

impl Abort for DownloadItemCallback {
    fn abort(&self) {
        self.cancel();
    }
}

impl Shared {
    pub(crate) fn new(url: String) -> Rc<Self> {
        Rc::new(Self {
            bounds: Cell::new(Bounds::default()),
            // CEF rejects a zero-sized view, so start out non-zero.
            view_size: Cell::new(size(px(800.), px(600.))),
            scale_factor: Cell::new(1.),
            view: FrameSlot::default(),
            popup: FrameSlot::default(),
            popup_visible: Cell::new(false),
            popup_rect: Cell::new(Bounds::default()),
            dirty: Cell::new(false),
            resized: Cell::new(false),
            title: RefCell::new(String::new()),
            url: RefCell::new(url),
            is_loading: Cell::new(false),
            can_go_back: Cell::new(false),
            can_go_forward: Cell::new(false),
            cursor: Cell::new(CursorStyle::Arrow),
            composition: RefCell::new(String::new()),
            composition_bounds: RefCell::new(Vec::new()),
            received_frame: Cell::new(false),
            browser: RefCell::new(None),
            events: RefCell::new(Vec::new()),
            js_dialogs: Pending::default(),
            file_dialogs: Pending::default(),
            downloads: Downloads::default(),
            closed: Cell::new(false),
        })
    }

    /// Whether the webview has gone; see [`Shared::close`].
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.get()
    }

    /// The webview is gone: cancel everything still waiting on it, and turn
    /// down whatever the page asks for from now on.
    pub(crate) fn close(&self) {
        self.closed.set(true);
        self.js_dialogs.cancel_all();
        self.file_dialogs.cancel_all();
        self.downloads.cancel_waiting();
    }

    /// Keeps a JavaScript dialog until the application answers it.
    pub(crate) fn add_js_dialog(&self, reply: JsDialogReply) -> DialogId {
        DialogId(self.js_dialogs.insert(reply))
    }

    /// Answers a JavaScript dialog. `false` when it is no longer waiting.
    pub(crate) fn resolve_js_dialog(&self, id: DialogId, accept: bool, text: Option<&str>) -> bool {
        // Taken out before answering: CEF may call straight back in.
        let Some(reply) = self.js_dialogs.take(id.0) else {
            return false;
        };
        reply.answer(accept, text);
        true
    }

    /// Keeps a file chooser until the application answers it.
    pub(crate) fn add_file_dialog(&self, reply: FileDialogReply) -> FileDialogId {
        FileDialogId(self.file_dialogs.insert(reply))
    }

    /// Answers a file chooser; `None` or no paths cancels it. `false` when it
    /// is no longer waiting.
    pub(crate) fn resolve_file_dialog(
        &self,
        id: FileDialogId,
        paths: Option<&[std::path::PathBuf]>,
    ) -> bool {
        let Some(reply) = self.file_dialogs.take(id.0) else {
            return false;
        };
        match paths {
            Some(paths) if !paths.is_empty() => reply.answer(paths),
            _ => reply.cancel(),
        }
        true
    }

    /// CEF asks where a download goes. Turned down at once when the webview
    /// is gone; otherwise kept for the application, which is told. Returns
    /// whether it was kept.
    pub(crate) fn add_download(&self, id: u32, decision: BeforeDownloadCallback) -> bool {
        if self.is_closed() {
            // Dropping the question unanswered is CEF's cancel.
            drop(decision);
            self.downloads.reject(id.into());
            return false;
        }
        self.downloads.request(id.into(), decision);
        true
    }

    /// CEF reports a download's progress. Returns whether the application
    /// should hear about it.
    pub(crate) fn update_download(
        &self,
        id: u32,
        control: DownloadItemCallback,
        finished: bool,
    ) -> bool {
        self.downloads.update(id.into(), control, finished)
    }

    /// Where a download goes, or `None` to cancel it. `false` when it is not
    /// waiting for an answer.
    pub(crate) fn resolve_download(&self, id: DownloadId, path: Option<&std::path::Path>) -> bool {
        self.downloads.resolve(id.0, path)
    }

    /// Dismisses every JavaScript dialog still waiting — the page navigated
    /// away, say, and they no longer mean anything.
    pub(crate) fn cancel_js_dialogs(&self) {
        self.js_dialogs.cancel_all();
    }

    /// Queue an event for the application; the pump emits it on its next pass.
    ///
    /// CEF reports a download's progress many times a second, so an update
    /// replaces one for the same download still waiting in the queue rather
    /// than piling up behind it.
    pub(crate) fn push_event(&self, event: WebviewEvent) {
        self.dirty.set(true);
        let mut events = self.events.borrow_mut();
        if let WebviewEvent::DownloadUpdated { id, .. } = &event {
            let queued = events.iter_mut().rev().find(|queued| {
                matches!(queued, WebviewEvent::DownloadUpdated {
                    id: queued,
                    state: DownloadState::InProgress,
                    ..
                } if queued == id)
            });
            if let Some(queued) = queued {
                *queued = event;
                return;
            }
        }
        events.push(event);
    }

    /// The events queued since the last call, oldest first.
    pub(crate) fn take_events(&self) -> Vec<WebviewEvent> {
        std::mem::take(&mut *self.events.borrow_mut())
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

    /// Lets go of the browser once the webview is gone. The browser holds the
    /// client, whose handlers hold this, so keeping it would keep all three
    /// alive for good.
    pub(crate) fn take_browser(&self) -> Option<Browser> {
        self.browser.borrow_mut().take()
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

        self.view.put(frame);
        self.dirty.set(true);
    }

    /// The popup layer CEF paints for `<select>` dropdowns and the like.
    pub(crate) fn put_popup_frame(&self, frame: Frame) {
        log::debug!("popup frame");
        self.popup.put(frame);
        self.dirty.set(true);
    }

    pub(crate) fn set_popup_visible(&self, visible: bool) {
        log::debug!("popup {}", if visible { "shown" } else { "hidden" });
        self.popup_visible.set(visible);
        if !visible {
            // Keeping the last frame around would leave a ghost dropdown behind.
            self.popup.clear();
        }
        self.dirty.set(true);
    }

    /// Where the popup goes, in the webview's own coordinate space.
    pub(crate) fn set_popup_rect(&self, rect: Bounds<Pixels>) {
        log::debug!("popup rect {rect:?}");
        self.popup_rect.set(rect);
        self.dirty.set(true);
    }

    pub(crate) fn with_frame<R>(&self, f: impl FnOnce(Option<&Frame>) -> R) -> R {
        f(self.view.frame.borrow().as_ref())
    }

    /// Runs `f` with the popup frame and its rect, but only while it is showing.
    pub(crate) fn with_popup<R>(&self, f: impl FnOnce(Option<(&Frame, Bounds<Pixels>)>) -> R) -> R {
        if !self.popup_visible.get() {
            return f(None);
        }
        let rect = self.popup_rect.get();
        f(self
            .popup
            .frame
            .borrow()
            .as_ref()
            .map(|frame| (frame, rect)))
    }

    /// Takes the stale frames that gpui should release from its texture cache.
    pub(crate) fn take_stale_cpu_frames(&self) -> Vec<Arc<RenderImage>> {
        [self.view.take_stale(), self.popup.take_stale()]
            .into_iter()
            .flatten()
            .collect()
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

    pub(crate) fn set_cursor(&self, cursor: CursorStyle) {
        if self.cursor.replace(cursor) != cursor {
            // The cursor is applied while painting, so a repaint has to happen.
            self.dirty.set(true);
        }
    }

    pub(crate) fn cursor(&self) -> CursorStyle {
        self.cursor.get()
    }

    /// Records the text the IME is composing, or clears it when `text` is empty.
    pub(crate) fn set_composition(&self, text: &str) {
        let mut composition = self.composition.borrow_mut();
        composition.clear();
        composition.push_str(text);
    }

    pub(crate) fn composition(&self) -> String {
        self.composition.borrow().clone()
    }

    /// The per-character rects CEF reports through `OnImeCompositionRangeChanged`.
    pub(crate) fn set_composition_bounds(&self, bounds: Vec<Bounds<Pixels>>) {
        *self.composition_bounds.borrow_mut() = bounds;
    }

    /// The rect covering `range` of the composition, in the webview's own
    /// coordinate space. Falls back to the whole composition when the range is
    /// out of step with what CEF last reported.
    pub(crate) fn composition_bounds(
        &self,
        range: std::ops::Range<usize>,
    ) -> Option<Bounds<Pixels>> {
        let bounds = self.composition_bounds.borrow();
        let slice = bounds.get(range).filter(|slice| !slice.is_empty());
        let slice = match slice {
            Some(slice) => slice,
            None if !bounds.is_empty() => &bounds[..],
            None => return None,
        };
        slice.iter().copied().reduce(|acc, rect| acc.union(&rect))
    }

    pub(crate) fn title(&self) -> String {
        self.title.borrow().clone()
    }

    pub(crate) fn url(&self) -> String {
        self.url.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, Bounds};

    fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(w), px(h)),
        }
    }

    #[test]
    fn first_layout_is_a_resize() {
        let shared = Shared::new("about:blank".into());
        shared.set_layout(bounds(0., 0., 640., 480.), 1.);

        assert!(shared.take_resized());
        assert_eq!(shared.view_size(), size(px(640.), px(480.)));
        // Taking it clears it.
        assert!(!shared.take_resized());
    }

    #[test]
    fn sub_pixel_jitter_does_not_resize() {
        let shared = Shared::new("about:blank".into());
        shared.set_layout(bounds(0., 0., 640., 480.), 1.);
        assert!(shared.take_resized());

        // Layout rounding must not make us call was_resized() every frame.
        shared.set_layout(bounds(0., 0., 640.4, 480.4), 1.);
        assert!(!shared.take_resized());

        shared.set_layout(bounds(0., 0., 642., 480.), 1.);
        assert!(shared.take_resized());
    }

    #[test]
    fn moving_without_resizing_updates_bounds_only() {
        let shared = Shared::new("about:blank".into());
        shared.set_layout(bounds(0., 0., 640., 480.), 1.);
        assert!(shared.take_resized());

        // Scrolling the webview around should not re-lay-out the page.
        shared.set_layout(bounds(100., 50., 640., 480.), 1.);
        assert!(!shared.take_resized());
        assert_eq!(shared.bounds().origin, point(px(100.), px(50.)));
    }

    #[test]
    fn scale_factor_change_is_a_resize() {
        let shared = Shared::new("about:blank".into());
        shared.set_layout(bounds(0., 0., 640., 480.), 1.);
        assert!(shared.take_resized());

        // Dragging the window to a Retina display has to reach CEF.
        shared.set_layout(bounds(0., 0., 640., 480.), 2.);
        assert!(shared.take_resized());
        assert_eq!(shared.scale_factor(), 2.);
    }

    #[test]
    fn zero_sized_layout_is_clamped() {
        let shared = Shared::new("about:blank".into());
        // A collapsed element must not ask CEF for a zero-sized view.
        shared.set_layout(bounds(0., 0., 0., 0.), 1.);

        assert_eq!(shared.view_size(), size(px(1.), px(1.)));
    }

    #[test]
    fn cursor_only_dirties_on_change() {
        let shared = Shared::new("about:blank".into());
        shared.take_dirty();

        shared.set_cursor(CursorStyle::PointingHand);
        assert!(shared.take_dirty());
        assert_eq!(shared.cursor(), CursorStyle::PointingHand);

        // CEF reports the cursor on every move; repeats must not force repaints.
        shared.set_cursor(CursorStyle::PointingHand);
        assert!(!shared.take_dirty());
    }

    #[test]
    fn title_and_url_track_the_page() {
        let shared = Shared::new("https://example.com/".into());
        assert_eq!(shared.url(), "https://example.com/");
        assert_eq!(shared.title(), "");

        shared.set_title("Example".into());
        shared.set_url("https://example.com/next".into());
        assert_eq!(shared.title(), "Example");
        assert_eq!(shared.url(), "https://example.com/next");
        assert!(shared.take_dirty());
    }

    #[test]
    fn load_state_round_trips() {
        let shared = Shared::new("about:blank".into());
        assert!(!shared.is_loading());
        assert!(!shared.can_go_back());

        shared.set_load_state(true, true, false);
        assert!(shared.is_loading());
        assert!(shared.can_go_back());
        assert!(!shared.can_go_forward());
        assert!(shared.take_dirty());
    }

    #[test]
    fn events_come_out_once_and_in_order() {
        let shared = Shared::new("about:blank".into());
        shared.take_dirty();

        shared.push_event(WebviewEvent::Message("first".into()));
        shared.push_event(WebviewEvent::Message("second".into()));
        // The pump only looks at a view that says something changed.
        assert!(shared.take_dirty());
        assert_eq!(
            shared.take_events(),
            [
                WebviewEvent::Message("first".into()),
                WebviewEvent::Message("second".into())
            ]
        );
        assert!(shared.take_events().is_empty());
    }

    #[test]
    fn download_progress_collapses_while_it_waits_in_the_queue() {
        let shared = Shared::new("about:blank".into());
        let update = |id, state, received_bytes| WebviewEvent::DownloadUpdated {
            id: DownloadId(id),
            state,
            received_bytes,
            total_bytes: Some(100),
            path: None,
        };
        shared.push_event(update(1, DownloadState::InProgress, 10));
        shared.push_event(update(2, DownloadState::InProgress, 5));
        shared.push_event(update(1, DownloadState::InProgress, 20));
        shared.push_event(update(1, DownloadState::Completed, 100));
        // Nothing replaces the end of a download.
        shared.push_event(update(1, DownloadState::InProgress, 0));

        assert_eq!(
            shared.take_events(),
            [
                update(1, DownloadState::Completed, 100),
                update(2, DownloadState::InProgress, 5),
                update(1, DownloadState::InProgress, 0),
            ]
        );
    }

    #[test]
    fn replacing_a_cpu_frame_hands_back_the_old_one() {
        let shared = Shared::new("about:blank".into());
        let first = cpu_frame(2, 2);
        let second = cpu_frame(2, 2);

        shared.put_frame(Frame::Cpu(first.clone()));
        // Nothing to release yet: gpui never saw a previous frame.
        assert!(shared.take_stale_cpu_frames().is_empty());

        shared.put_frame(Frame::Cpu(second));
        // The old texture has to come back so it can leave gpui's cache.
        let stale = shared.take_stale_cpu_frames();
        assert_eq!(stale.len(), 1);
        assert!(Arc::ptr_eq(&stale[0], &first));
        assert!(shared.take_stale_cpu_frames().is_empty());
    }

    #[test]
    fn the_popup_is_only_drawn_while_it_is_showing() {
        let shared = Shared::new("about:blank".into());
        shared.put_popup_frame(Frame::Cpu(cpu_frame(4, 4)));
        shared.set_popup_rect(bounds(10., 20., 100., 200.));

        // CEF paints the popup layer before it announces it, so a frame alone
        // must not put a stray dropdown on screen.
        assert!(shared.with_popup(|popup| popup.is_none()));

        shared.set_popup_visible(true);
        let rect = shared.with_popup(|popup| popup.map(|(_, rect)| rect));
        assert_eq!(rect, Some(bounds(10., 20., 100., 200.)));
    }

    #[test]
    fn hiding_the_popup_releases_its_texture() {
        let shared = Shared::new("about:blank".into());
        let frame = cpu_frame(4, 4);
        shared.put_popup_frame(Frame::Cpu(frame.clone()));
        shared.set_popup_visible(true);
        shared.take_stale_cpu_frames();

        shared.set_popup_visible(false);

        // Otherwise the dropdown would stay on screen after it closed, and its
        // texture would leak in gpui's cache.
        assert!(shared.with_popup(|popup| popup.is_none()));
        let stale = shared.take_stale_cpu_frames();
        assert_eq!(stale.len(), 1);
        assert!(Arc::ptr_eq(&stale[0], &frame));
    }

    fn cpu_frame(width: u32, height: u32) -> Arc<RenderImage> {
        let buffer = image::ImageBuffer::from_pixel(width, height, image::Rgba([0u8, 0, 0, 255]));
        Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
    }
}
