//! A demo that embeds a real Chromium inside a gpui window.
//!
//! The point of it is that the webview behaves like any other gpui element:
//!
//! - **It is clipped by rounded corners.** Attaching a child view (wry's
//!   `WKWebView`, for instance) only ever gives you a rectangle.
//! - **gpui elements are drawn on top of it.** The badge in the bottom right
//!   sits in front of the page.
//! - **It shares the layout tree.** Toggling the side panel changes the
//!   webview's width, and CEF's render size follows.
//!
//! The toolbar is drawn by gpui, and whether the history buttons are enabled and
//! whether a load is in flight come straight from CEF's `LoadHandler`.

use gpui::{
    div, prelude::*, px, rgb, rgba, size, Application, Bounds, Context, Entity, FocusHandle,
    Focusable, FontWeight, KeyDownEvent, MouseButton, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
};
use gpui_cef::{RuntimeOptions, Webview, WebviewOptions};

const START_URL: &str = "https://voluntas.ghost.io/gpui-webview/";

/// Pages reachable from the side panel.
const BOOKMARKS: &[(&str, &str)] = &[
    (
        "The gpui-webview post",
        "https://voluntas.ghost.io/gpui-webview/",
    ),
    ("Zed", "https://zed.dev/"),
    ("CEF", "https://bitbucket.org/chromiumembedded/cef/"),
    ("Rust", "https://www.rust-lang.org/"),
];

// Colors. gpui has no theming machinery, so this demo keeps them as constants.
const BG: u32 = 0x16161a;
const PANEL: u32 = 0x1e1e24;
const BORDER: u32 = 0x33333d;
const TEXT: u32 = 0xe8e8ec;
const TEXT_DIM: u32 = 0x8a8a96;
const ACCENT: u32 = 0x6c9cff;

struct BrowserWindow {
    webview: Entity<Webview>,
    /// What the address bar is being edited to. `None` means show the webview's
    /// current URL instead.
    editing_address: Option<String>,
    address_focus: FocusHandle,
    show_panel: bool,
    inverted: bool,
}

impl BrowserWindow {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let options = WebviewOptions {
            // GPUI_CEF_ACCELERATED=1 tries the shared IOSurface path. Stock gpui
            // cannot draw a BGRA surface, so it will crash there.
            accelerated: std::env::var("GPUI_CEF_ACCELERATED").is_ok(),
            ..WebviewOptions::url(START_URL)
        };
        let webview = cx.new(|cx| Webview::new(window, cx, options));

        // Repaint the toolbar when the title, URL, or load state changes.
        cx.observe(&webview, |_, _, cx| cx.notify()).detach();

        Self {
            webview,
            editing_address: None,
            address_focus: cx.focus_handle(),
            show_panel: true,
            inverted: false,
        }
    }

    /// What the address bar should show: the edit buffer while editing, the
    /// current URL otherwise.
    fn address_text(&self, cx: &Context<Self>) -> String {
        self.editing_address
            .clone()
            .unwrap_or_else(|| self.webview.read(cx).url())
    }

    fn navigate(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let url = normalize_url(url);
        self.webview.read(cx).load_url(&url);
        self.editing_address = None;
        // Hand the keyboard back to the page.
        let focus = self.webview.read(cx).focus_handle(cx);
        window.focus(&focus);
        cx.notify();
    }

    fn on_address_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;

        match keystroke.key.as_str() {
            "enter" => {
                let url = self.address_text(cx);
                self.navigate(&url, window, cx);
                return;
            }
            "escape" => {
                self.editing_address = None;
                let focus = self.webview.read(cx).focus_handle(cx);
                window.focus(&focus);
                cx.notify();
                return;
            }
            "backspace" => {
                let mut text = self.address_text(cx);
                text.pop();
                self.editing_address = Some(text);
                cx.notify();
                return;
            }
            _ => {}
        }

        // Shortcuts such as Cmd-A are not text input.
        if keystroke.modifiers.platform || keystroke.modifiers.control {
            return;
        }
        if let Some(input) = keystroke.key_char.as_deref() {
            let mut text = self.address_text(cx);
            text.push_str(input);
            self.editing_address = Some(text);
            cx.notify();
        }
    }

    /// A toolbar button. When `enabled` is false, clicking does nothing.
    fn toolbar_button(
        &self,
        id: &'static str,
        label: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> gpui::AnyElement {
        let base = div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .size(px(30.))
            .rounded_md()
            .text_color(if enabled { rgb(TEXT) } else { rgb(TEXT_DIM) })
            .child(label);

        if enabled {
            base.cursor_pointer()
                .hover(|style| style.bg(rgb(BORDER)))
                .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
                .into_any_element()
        } else {
            base.into_any_element()
        }
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let webview = self.webview.read(cx);
        let can_go_back = webview.can_go_back();
        let can_go_forward = webview.can_go_forward();
        let is_loading = webview.is_loading();
        let address = self.address_text(cx);
        let editing = self.editing_address.is_some();

        div()
            .flex()
            .items_center()
            .gap_1()
            .h(px(48.))
            .px_3()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                self.toolbar_button("back", "\u{2190}", can_go_back, cx, |this, _, cx| {
                    this.webview.read(cx).go_back()
                }),
            )
            .child(
                self.toolbar_button("forward", "\u{2192}", can_go_forward, cx, |this, _, cx| {
                    this.webview.read(cx).go_forward()
                }),
            )
            .child(self.toolbar_button(
                "reload",
                if is_loading { "\u{2715}" } else { "\u{27f3}" },
                true,
                cx,
                move |this, _, cx| {
                    let webview = this.webview.read(cx);
                    if webview.is_loading() {
                        webview.stop();
                    } else {
                        webview.reload();
                    }
                },
            ))
            .child(
                // The address bar. gpui ships no text input widget, so this is a
                // focusable div that collects key events by hand.
                div()
                    .id("address")
                    .track_focus(&self.address_focus)
                    .flex()
                    .flex_1()
                    .items_center()
                    .h(px(32.))
                    .px_3()
                    .mx_2()
                    .rounded_md()
                    .bg(rgb(BG))
                    .border_1()
                    .border_color(if editing { rgb(ACCENT) } else { rgb(BORDER) })
                    .text_sm()
                    .text_color(rgb(TEXT))
                    .cursor_text()
                    .on_key_down(cx.listener(Self::on_address_key))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            window.focus(&this.address_focus);
                            // Start editing from whatever URL is showing.
                            if this.editing_address.is_none() {
                                this.editing_address = Some(this.webview.read(cx).url());
                            }
                            cx.notify();
                        }),
                    )
                    .child(SharedString::from(address))
                    // Stand-in for a caret while editing.
                    .when(editing, |this| {
                        this.child(div().w(px(1.)).h(px(16.)).bg(rgb(ACCENT)))
                    }),
            )
            .child(
                div()
                    .w(px(72.))
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child(if is_loading { "Loading..." } else { "" }),
            )
            .child(
                self.toolbar_button("panel", "\u{25a4}", true, cx, |this, _, cx| {
                    this.show_panel = !this.show_panel;
                    cx.notify();
                }),
            )
    }

    fn render_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let webview = self.webview.read(cx);
        let title = webview.title();
        let url = webview.url();

        div()
            .flex()
            .flex_col()
            .gap_4()
            .w(px(260.))
            .h_full()
            .p_4()
            .border_l_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_DIM))
                            .child("Current page"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child(SharedString::from(title)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_DIM))
                            .child(SharedString::from(url)),
                    ),
            )
            .child(div().h(px(1.)).bg(rgb(BORDER)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xs().text_color(rgb(TEXT_DIM)).child("Bookmarks"))
                    .children(BOOKMARKS.iter().map(|(label, url)| {
                        div()
                            .id(*label)
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigate(url, window, cx)
                            }))
                            .child(*label)
                    })),
            )
            .child(div().h(px(1.)).bg(rgb(BORDER)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_DIM))
                            .child("Inject JavaScript into the page"),
                    )
                    .child(
                        div()
                            .id("invert")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(ACCENT))
                            .text_sm()
                            .text_color(rgb(0x101014))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.inverted = !this.inverted;
                                let filter = if this.inverted { "invert(1)" } else { "none" };
                                this.webview.read(cx).eval(&format!(
                                    "document.documentElement.style.filter = '{filter}'"
                                ));
                                cx.notify();
                            }))
                            .child(if self.inverted {
                                "Restore colors"
                            } else {
                                "Invert colors"
                            }),
                    ),
            )
    }
}

impl Render for BrowserWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = self.render_toolbar(cx);
        let panel = if self.show_panel {
            Some(self.render_panel(cx).into_any_element())
        } else {
            None
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .child(toolbar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        div().relative().flex_1().p_3().child(
                            // Clip the webview to rounded corners and draw a gpui
                            // element over it. A child-view backend can do
                            // neither.
                            div()
                                .relative()
                                .size_full()
                                .rounded_lg()
                                .overflow_hidden()
                                .border_1()
                                .border_color(rgb(BORDER))
                                .child(div().size_full().child(self.webview.clone()))
                                .child(
                                    div()
                                        .absolute()
                                        .right(px(12.))
                                        .bottom(px(12.))
                                        .px_3()
                                        .py_1()
                                        .rounded_md()
                                        .bg(rgba(0x000000cc))
                                        .text_xs()
                                        .text_color(rgb(TEXT))
                                        .child("A gpui element, drawn over the webview"),
                                ),
                        ),
                    )
                    .children(panel),
            )
    }
}

/// Adds a scheme when one is missing. Input without a `.` is treated as a search.
fn normalize_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.contains("://") || trimmed.starts_with("about:") {
        trimmed.to_string()
    } else if trimmed.contains('.') && !trimmed.contains(' ') {
        format!("https://{trimmed}")
    } else {
        format!("https://duckduckgo.com/?q={}", urlencode(trimmed))
    }
}

/// Just enough percent-encoding to put a search term in a query string.
fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn main() {
    env_logger::init();

    // 1. At the top of main: load CEF into the process. The real initialization
    //    happens inside run().
    let runtime = match gpui_cef::init(RuntimeOptions {
        remote_debugging_port: Some(9229),
        log_file: std::env::var("GPUI_CEF_LOG").ok().map(Into::into),
        ..Default::default()
    }) {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("failed to start the browser runtime: {err}");
            std::process::exit(1);
        }
    };

    // Hand the closure a clone. Moving the value in would run cef_shutdown() the
    // moment the closure returns, taking the process with it.
    let cef = runtime.clone();
    Application::new().run(move |cx| {
        // 2. gpui has created NSApp by now, so initialize CEF here. The other
        //    order lets CEF claim the NSApplication singleton and gpui crashes.
        if let Err(err) = cef.start(cx) {
            eprintln!("failed to initialize the browser runtime: {err}");
            std::process::exit(1);
        }

        let bounds = Bounds::centered(None, size(px(1280.), px(860.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("gpui-cef".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| BrowserWindow::new(window, cx)),
        )
        .unwrap();
        cx.activate(true);
    });

    // 3. run() returned, so the message loop is done: shut CEF down.
    drop(runtime);
}
