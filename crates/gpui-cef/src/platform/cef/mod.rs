//! macOS backend: render Chromium (CEF) off-screen and composite it into gpui.
//!
//! # How it fits together
//!
//! 1. CEF is brought up in two steps, [`init`] then [`Runtime::start`], to avoid
//!    fighting gpui over `NSApp`. See [`Runtime`] for why.
//! 2. [`Webview::new`] creates a windowless browser. CEF then reports every
//!    repaint through the handlers in [`handlers`].
//! 3. Frames arrive either as a CPU-side BGRA buffer (`OnPaint`, the default) or
//!    as a shared IOSurface (`OnAcceleratedPaint`, opt-in). Both land in
//!    [`shared::Shared`], and [`WebviewSurface`] draws whichever is current.
//! 4. CEF's message loop is driven by a `CFRunLoopTimer` installed on the main
//!    run loop; see [`pump`] for why it cannot live on gpui's executor.

mod handlers;
mod input;
mod nsapp;
mod pump;
mod shared;

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
};

use cef::{
    args::Args, browser_host_create_browser_sync, execute_process, initialize,
    library_loader::LibraryLoader, Browser, BrowserHost, BrowserSettings, ImplBrowser,
    ImplBrowserHost, ImplFrame, Settings, WindowInfo,
};
use gpui::{
    div, point, px, relative, surface, AnyElement, App, Bounds, Context, Element, ElementId,
    Focusable, GlobalElementId, Hitbox, HitboxBehavior, ImageSource, InspectorElementId,
    InteractiveElement, IntoElement, KeyDownEvent, KeyUpEvent, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, ParentElement, Pixels, Render,
    ScrollDelta, ScrollWheelEvent, Style, Styled, StyledImage, Window,
};

use self::shared::{Frame, Shared};
use crate::{Error, RuntimeOptions, WebviewOptions};

/// Pixels to scroll per wheel line, used to convert gpui's `ScrollDelta::Lines`.
const LINE_HEIGHT: f32 = 20.;

/// The process-wide CEF runtime, as a cloneable handle.
///
/// Starting CEF is split in two:
///
/// 1. [`init`] — at the top of `main`. Loads the framework and handles the case
///    where this process was spawned as a CEF child process.
/// 2. [`Runtime::start`] — inside the `Application::run` closure. This is what
///    actually calls `cef_initialize`.
///
/// The split exists because of `NSApp`. macOS has exactly one `NSApplication`
/// per process, and it is instantiated as **whichever class calls
/// `sharedApplication` first**. Initializing CEF first yields a plain
/// `NSApplication`, and gpui then crashes because the instance it gets back has
/// none of its own ivars. gpui has to create `NSApp` before CEF is initialized.
///
/// The handle is cloneable so that moving it into a closure does not shorten its
/// life. Handing the value itself to the `Application::run` closure would run
/// `cef_shutdown()` **the moment the closure returns** — that is, right after
/// startup — killing Chromium mid-launch with a SIGTRAP.
#[derive(Clone)]
pub struct Runtime(Rc<RuntimeInner>);

struct RuntimeInner {
    // Handle to the dlopen'd framework. Dropping it takes CEF down with it.
    _loader: LibraryLoader,
    app: RefCell<cef::App>,
    options: RuntimeOptions,
    started: Cell<bool>,
}

impl Runtime {
    /// Calls `cef_initialize`. Call it exactly once, **inside the
    /// `gpui::Application::run` closure** and before creating the first
    /// [`Webview`].
    ///
    /// `cx` is unused; it is taken so the signature pins the call site to "after
    /// gpui has created `NSApp`".
    pub fn start(&self, _cx: &mut App) -> crate::Result<()> {
        if self.0.started.replace(true) {
            return Ok(());
        }

        // Retrofit CefAppProtocol onto gpui's NSApplication subclass. Without
        // this, a CHECK inside cef_initialize takes the process down.
        if let Err(err) = nsapp::adopt_cef_app_protocol() {
            return Err(Error::Backend(err.to_string()));
        }

        let args = Args::new();
        let mut settings = Settings {
            windowless_rendering_enabled: 1,
            // gpui owns the event loop — gpui is what calls [NSApp run]. Without
            // external_message_pump, CEF builds a message pump that assumes it
            // drives NSApplication's run loop itself, and Chromium then trips a
            // CHECK that takes the whole process down.
            multi_threaded_message_loop: 0,
            external_message_pump: 1,
            ..Default::default()
        };
        if let Some(log_file) = &self.0.options.log_file {
            settings.log_file = log_file.to_string_lossy().as_ref().into();
            settings.log_severity = cef::LogSeverity::VERBOSE;
        }
        if let Some(cache) = &self.0.options.cache_path {
            settings.root_cache_path = cache.to_string_lossy().as_ref().into();
            settings.cache_path = cache.to_string_lossy().as_ref().into();
        }

        let mut app = self.0.app.borrow_mut();
        if initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut *app),
            std::ptr::null_mut(),
        ) != 1
        {
            return Err(Error::Initialize);
        }

        Ok(())
    }

    /// Shuts CEF down. Call this once `gpui::Application::run` has returned.
    ///
    /// This drops one handle; `cef_shutdown()` runs when the last one goes, so
    /// keeping a clone alive past this call keeps CEF alive too.
    pub fn shutdown(self) {}
}

impl Drop for RuntimeInner {
    fn drop(&mut self) {
        if self.started.get() {
            cef::shutdown();
        }
    }
}

/// Loads CEF into the process. Call this **at the top of `main`**, before
/// `gpui::Application::new()`. The actual initialization happens in
/// [`Runtime::start`].
///
/// On macOS the executable has to live inside an `.app` bundle that ships
/// `Frameworks/Chromium Embedded Framework.framework` — the layout `xtask`
/// produces.
pub fn init(options: RuntimeOptions) -> crate::Result<Runtime> {
    // Loading the framework and running execute_process twice is not something
    // CEF expects, and the failure mode is a confusing crash much later.
    static INITIALIZED: AtomicBool = AtomicBool::new(false);
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return Err(Error::Backend("gpui_cef::init() called twice".into()));
    }

    let exe = std::env::current_exe().map_err(|e| Error::LibraryLoad(e.to_string()))?;
    let loader = LibraryLoader::new(&exe, false);
    if !loader.load() {
        return Err(Error::LibraryLoad(exe.display().to_string()));
    }

    // Pin the API version. This has to happen before any handler is registered.
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);

    let args = Args::new();
    let mut app = handlers::GpuiCefApp::new(handlers::AppConfig {
        remote_debugging_port: options.remote_debugging_port,
        log_to_stderr: options.log_to_stderr,
    });

    // Returns -1 in the browser process, which is what this one is. Child
    // processes run a separate binary (`*_helper`), so they never get here.
    let code = execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if code != -1 {
        // Spawned as a child process after all — exit with whatever CEF wants.
        std::process::exit(code);
    }

    Ok(Runtime(Rc::new(RuntimeInner {
        _loader: loader,
        app: RefCell::new(app),
        options,
        started: Cell::new(false),
    })))
}

/// A webview, as a gpui entity.
///
/// Build it with
/// `cx.new(|cx| Webview::new(window, cx, WebviewOptions::url("https://example.com")))`
/// and place it like any other element.
pub struct Webview {
    shared: Rc<Shared>,
    browser: Option<Browser>,
    focus_handle: gpui::FocusHandle,
    _pump: pump::Registration,
    _focus_subscriptions: [gpui::Subscription; 2],
}

impl Webview {
    /// Creates the webview and starts loading the first page.
    pub fn new(window: &mut Window, cx: &mut Context<Self>, options: WebviewOptions) -> Self {
        let shared = Shared::new(options.url.clone());
        // Provisional size until layout runs. CEF refuses to create a browser
        // with a zero-sized view.
        shared.set_layout(
            Bounds {
                origin: point(px(0.), px(0.)),
                size: window.viewport_size(),
            },
            window.scale_factor(),
        );

        let accelerated = options.accelerated;
        let window_info = WindowInfo {
            windowless_rendering_enabled: 1,
            // Receive frames through a shared texture (IOSurface).
            shared_texture_enabled: accelerated as _,
            // With a shared texture we also drive frame production ourselves.
            external_begin_frame_enabled: accelerated as _,
            ..Default::default()
        };

        let browser_settings = BrowserSettings {
            windowless_frame_rate: options.frame_rate,
            background_color: if options.transparent {
                0
            } else {
                0xFF_FF_FF_FF
            },
            ..Default::default()
        };

        let mut client = handlers::GpuiCefClient::build(shared.clone());
        let browser = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&options.url.as_str().into()),
            Some(&browser_settings),
            None,
            None,
        );

        match browser.clone() {
            Some(browser) => shared.set_browser(browser),
            None => log::error!("cef_browser_host_create_browser_sync() returned null"),
        }

        let pump = pump::register(
            shared.clone(),
            accelerated,
            cx.entity().downgrade(),
            cx.to_async(),
        );

        // CEF has to be told when focus moves, or a caret keeps blinking in the
        // page after the user clicks something else in the app.
        let focus_handle = cx.focus_handle();
        let focus_subscriptions = [
            cx.on_focus_in(&focus_handle, window, |this, _, _| {
                this.set_browser_focus(true)
            }),
            cx.on_focus_out(&focus_handle, window, |this, _, _, _| {
                this.set_browser_focus(false)
            }),
        ];

        Self {
            shared,
            browser,
            focus_handle,
            _pump: pump,
            _focus_subscriptions: focus_subscriptions,
        }
    }

    fn set_browser_focus(&self, focused: bool) {
        if let Some(host) = self.host() {
            host.set_focus(focused as i32);
        }
    }

    /// Navigates to another URL.
    pub fn load_url(&self, url: &str) {
        if let Some(frame) = self.browser.as_ref().and_then(|b| b.main_frame()) {
            frame.load_url(Some(&url.into()));
        }
    }

    /// Reloads the current page.
    pub fn reload(&self) {
        if let Some(browser) = &self.browser {
            browser.reload();
        }
    }

    /// Goes back in history.
    pub fn go_back(&self) {
        if let Some(browser) = &self.browser {
            browser.go_back();
        }
    }

    /// Goes forward in history.
    pub fn go_forward(&self) {
        if let Some(browser) = &self.browser {
            browser.go_forward();
        }
    }

    /// The current page title.
    pub fn title(&self) -> String {
        self.shared.title()
    }

    /// The current URL.
    pub fn url(&self) -> String {
        self.shared.url()
    }

    /// Whether a page is currently loading.
    pub fn is_loading(&self) -> bool {
        self.shared.is_loading()
    }

    /// Whether there is history to go back to.
    pub fn can_go_back(&self) -> bool {
        self.shared.can_go_back()
    }

    /// Whether there is history to go forward to.
    pub fn can_go_forward(&self) -> bool {
        self.shared.can_go_forward()
    }

    /// Runs JavaScript in the page.
    pub fn eval(&self, script: &str) {
        if let Some(frame) = self.browser.as_ref().and_then(|b| b.main_frame()) {
            frame.execute_java_script(Some(&script.into()), None, 0);
        }
    }

    /// Stops the current load.
    pub fn stop(&self) {
        if let Some(browser) = &self.browser {
            browser.stop_load();
        }
    }

    fn host(&self) -> Option<BrowserHost> {
        self.browser.as_ref().and_then(|b| b.host())
    }

    fn send_mouse_move(&self, event: &MouseMoveEvent) {
        let Some(host) = self.host() else { return };
        let flags =
            input::with_pressed_button(input::modifiers(&event.modifiers), event.pressed_button);
        let cef_event = input::mouse_event(event.position, self.shared.bounds(), flags);
        host.send_mouse_move_event(Some(&cef_event), 0);
    }

    fn send_mouse_down(&self, event: &MouseDownEvent) {
        let Some(host) = self.host() else { return };
        let Some(button) = input::mouse_button(event.button) else {
            return;
        };
        let flags =
            input::with_pressed_button(input::modifiers(&event.modifiers), Some(event.button));
        let cef_event = input::mouse_event(event.position, self.shared.bounds(), flags);
        host.send_mouse_click_event(Some(&cef_event), button, 0, event.click_count as i32);
    }

    fn send_mouse_up(&self, event: &MouseUpEvent) {
        let Some(host) = self.host() else { return };
        let Some(button) = input::mouse_button(event.button) else {
            return;
        };
        let flags = input::modifiers(&event.modifiers);
        let cef_event = input::mouse_event(event.position, self.shared.bounds(), flags);
        host.send_mouse_click_event(Some(&cef_event), button, 1, event.click_count as i32);
    }

    fn send_scroll(&self, event: &ScrollWheelEvent) {
        let Some(host) = self.host() else { return };
        let flags = input::modifiers(&event.modifiers);
        let cef_event = input::mouse_event(event.position, self.shared.bounds(), flags);
        let (dx, dy) = match event.delta {
            ScrollDelta::Pixels(delta) => (f32::from(delta.x), f32::from(delta.y)),
            ScrollDelta::Lines(delta) => (delta.x * LINE_HEIGHT, delta.y * LINE_HEIGHT),
        };
        host.send_mouse_wheel_event(Some(&cef_event), dx.round() as i32, dy.round() as i32);
    }

    fn send_key_down(&self, event: &KeyDownEvent) {
        let Some(host) = self.host() else { return };
        for cef_event in input::key_down_events(&event.keystroke) {
            host.send_key_event(Some(&cef_event));
        }
    }

    fn send_key_up(&self, event: &KeyUpEvent) {
        let Some(host) = self.host() else { return };
        host.send_key_event(Some(&input::key_up_event(&event.keystroke)));
    }
}

impl Drop for Webview {
    fn drop(&mut self) {
        if let Some(host) = self.host() {
            host.close_browser(1);
        }
    }
}

impl Focusable for Webview {
    fn focus_handle(&self, _cx: &App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Webview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .key_context("Webview")
            .size_full()
            .overflow_hidden()
            .on_mouse_move(
                cx.listener(|this, event: &MouseMoveEvent, _, _| this.send_mouse_move(event)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, _| {
                    // Focusing raises on_focus_in, which is what tells CEF.
                    window.focus(&this.focus_handle);
                    this.send_mouse_down(event);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, _, _| this.send_mouse_down(event)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, _| this.send_mouse_up(event)),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, event: &MouseUpEvent, _, _| this.send_mouse_up(event)),
            )
            .on_scroll_wheel(
                cx.listener(|this, event: &ScrollWheelEvent, _, _| this.send_scroll(event)),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, _| this.send_key_down(event)))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, _| this.send_key_up(event)))
            .child(WebviewSurface {
                shared: self.shared.clone(),
            })
    }
}

/// Draws the most recent frame and writes the laid-out rect back to [`Shared`].
///
/// The rect is needed for two things:
/// - answering CEF's `get_view_rect`, i.e. what size it should render at
/// - translating window-space mouse positions into view space
struct WebviewSurface {
    shared: Rc<Shared>,
}

impl WebviewSurface {
    fn build_child(&self) -> Option<AnyElement> {
        let page = self.shared.with_frame(|frame| frame.map(frame_element))?;

        // CEF paints `<select>` dropdowns and similar into a separate layer with
        // its own rect, so they have to be composited over the page here.
        let popup = self.shared.with_popup(|popup| {
            popup.map(|(frame, rect)| {
                div()
                    .absolute()
                    .left(rect.origin.x)
                    .top(rect.origin.y)
                    .w(rect.size.width)
                    .h(rect.size.height)
                    .child(frame_element(frame))
                    .into_any_element()
            })
        });

        Some(match popup {
            Some(popup) => div()
                .relative()
                .size_full()
                .child(page)
                .child(popup)
                .into_any_element(),
            None => page,
        })
    }
}

/// Draws one CEF layer, whichever way its frame arrived.
fn frame_element(frame: &Frame) -> AnyElement {
    match frame {
        Frame::Accelerated(buffer) => surface(buffer.clone())
            .object_fit(ObjectFit::Fill)
            .size_full()
            .into_any_element(),
        Frame::Cpu(image) => gpui::img(ImageSource::Render(image.clone()))
            .object_fit(ObjectFit::Fill)
            .size_full()
            .into_any_element(),
    }
}

impl Element for WebviewSurface {
    type RequestLayoutState = Option<AnyElement>;
    /// The hitbox the page's cursor choice is attached to.
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();

        let mut child = self.build_child();
        let layout_id = match child.as_mut() {
            Some(child) => {
                let child_id = child.request_layout(window, cx);
                window.request_layout(style, [child_id], cx)
            }
            None => window.request_layout(style, [], cx),
        };
        (layout_id, child)
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        child: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        self.shared.set_layout(bounds, window.scale_factor());
        if let Some(child) = child.as_mut() {
            child.prepaint(window, cx);
        }
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        child: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(child) = child.as_mut() {
            child.paint(window, cx);
        }
        // Off-screen rendering means CEF never sets the real cursor, so whatever
        // the page asked for is applied here.
        window.set_cursor_style(self.shared.cursor(), hitbox);
    }
}

impl IntoElement for WebviewSurface {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
