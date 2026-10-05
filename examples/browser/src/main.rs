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
//!
//! What a windowless browser cannot do by itself comes to the application as a
//! [`WebviewEvent`], and the demo answers each one natively: gpui prompts for
//! JavaScript dialogs, open and save panels for file choosers and downloads,
//! the system browser for popups, a Reload banner when the renderer dies. The
//! "Feature test page" bookmark has a button for each.

mod demo;

use std::{collections::VecDeque, path::PathBuf, sync::Arc};

use gpui::{
    div, prelude::*, px, rgb, rgba, size, Bounds, Context, Entity, FocusHandle, Focusable,
    FontWeight, KeyDownEvent, MouseButton, PathPromptOptions, PromptButton, PromptLevel,
    SharedString, Subscription, TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use gpui_cef::{
    DialogId, DownloadId, FileDialogId, FileDialogMode, JsDialogKind, PdfOptions, RuntimeOptions,
    Webview, WebviewEvent, WebviewOptions,
};

use demo::Demo;

const START_URL: &str = "https://voluntas.ghost.io/gpui-webview/";

/// How many lines the side panel's event log keeps.
const EVENT_LOG_LINES: usize = 12;

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
const WARNING: u32 = 0xffb86c;

struct BrowserWindow {
    webview: Entity<Webview>,
    /// What the address bar is being edited to. `None` means show the webview's
    /// current URL instead.
    editing_address: Option<String>,
    address_focus: FocusHandle,
    show_panel: bool,
    inverted: bool,
    demo: Demo,
    /// The feature test page, once it has been written out.
    features_url: Option<String>,
    /// The latest events, newest last.
    events: VecDeque<String>,
    /// Set when the renderer died, until the page is reloaded.
    crashed: Option<String>,
    /// A `prompt()` being answered in the overlay.
    text_prompt: Option<TextPrompt>,
    _events: Subscription,
}

/// The overlay that answers `prompt()`: gpui's native prompt has buttons but
/// no text field.
struct TextPrompt {
    id: DialogId,
    message: String,
    text: String,
    focus: FocusHandle,
}

impl BrowserWindow {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let demo = Demo::from_env();
        let features_url = demo
            .features_url()
            .map_err(|err| log::error!("could not write the feature test page: {err}"))
            .ok();
        let start_url = match std::env::var("GPUI_CEF_START_URL") {
            Ok(url) if url == "features" => features_url.clone(),
            Ok(url) => Some(url),
            Err(_) => None,
        };

        let options = WebviewOptions {
            // GPUI_CEF_ACCELERATED=1 tries the shared IOSurface path. Stock gpui
            // cannot draw a BGRA surface, so it will crash there.
            accelerated: std::env::var("GPUI_CEF_ACCELERATED").is_ok(),
            navigation_filter: Some(Arc::new(demo::allow_navigation)),
            ..WebviewOptions::url(start_url.as_deref().unwrap_or(START_URL))
        };
        let webview = cx.new(|cx| Webview::new(window, cx, options));

        // Repaint the toolbar when the title, URL, or load state changes.
        cx.observe(&webview, |_, _, cx| cx.notify()).detach();
        let events = cx.subscribe_in(&webview, window, Self::on_webview_event);

        Self {
            webview,
            editing_address: None,
            address_focus: cx.focus_handle(),
            show_panel: true,
            inverted: false,
            demo,
            features_url,
            events: VecDeque::new(),
            crashed: None,
            text_prompt: None,
            _events: events,
        }
    }

    /// Everything the webview hands to the application lands here.
    fn on_webview_event(
        &mut self,
        _webview: &Entity<Webview>,
        event: &WebviewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::info!("webview event: {event:?}");
        if self.events.len() == EVENT_LOG_LINES {
            self.events.pop_front();
        }
        self.events.push_back(demo::describe(event));
        cx.notify();

        match event {
            WebviewEvent::Message(message) => self.on_page_message(message, cx),
            WebviewEvent::PopupRequested { url, .. } => {
                // A blocked popup goes to the system browser instead.
                if !self.demo.auto && !url.is_empty() {
                    cx.open_url(url);
                }
            }
            WebviewEvent::JsDialog {
                id,
                kind,
                message,
                default_prompt,
                ..
            } => self.answer_js_dialog(*id, *kind, message, default_prompt, window, cx),
            WebviewEvent::FileDialog {
                id,
                mode,
                default_path,
                ..
            } => self.answer_file_dialog(*id, *mode, default_path.clone(), cx),
            WebviewEvent::DownloadRequested {
                id, suggested_name, ..
            } => self.answer_download(*id, suggested_name, cx),
            WebviewEvent::RenderProcessTerminated { status, .. } => {
                if self.demo.auto {
                    self.webview.read(cx).reload();
                } else {
                    self.crashed = Some(format!("{status:?}"));
                }
            }
            WebviewEvent::PdfPrinted { path, ok: true, .. } if !self.demo.auto => {
                cx.reveal_path(path);
            }
            _ => {}
        }
    }

    /// The feature test page asks for things only the application can do.
    fn on_page_message(&mut self, message: &str, cx: &mut Context<Self>) {
        match message {
            "reload" => self.webview.read(cx).reload(),
            "crash" => self.crash_renderer(cx),
            "print-pdf" => self.print_to_pdf(cx),
            _ => {}
        }
    }

    /// `alert`, `confirm` and "leave this page?" get gpui's native prompt;
    /// `prompt` gets the overlay, since that needs a text field.
    fn answer_js_dialog(
        &mut self,
        id: DialogId,
        kind: JsDialogKind,
        message: &str,
        default_prompt: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.demo.auto {
            let accept = !demo::asks_to_decline(message);
            let text = (kind == JsDialogKind::Prompt).then(|| "from gpui".to_string());
            self.webview.read(cx).resolve_dialog(id, accept, text);
            return;
        }

        let answer = match kind {
            JsDialogKind::Prompt => {
                let focus = cx.focus_handle();
                window.focus(&focus, cx);
                self.text_prompt = Some(TextPrompt {
                    id,
                    message: message.to_string(),
                    text: default_prompt.to_string(),
                    focus,
                });
                return;
            }
            JsDialogKind::Alert => window.prompt(PromptLevel::Info, message, None, &["OK"], cx),
            JsDialogKind::Confirm => window.prompt(
                PromptLevel::Info,
                message,
                None,
                &[PromptButton::ok("OK"), PromptButton::cancel("Cancel")],
                cx,
            ),
            JsDialogKind::BeforeUnload { is_reload } => {
                let (question, go) = if is_reload {
                    ("Reload this page?", "Reload")
                } else {
                    ("Leave this page?", "Leave")
                };
                window.prompt(
                    PromptLevel::Warning,
                    question,
                    Some("Changes you made may not be saved."),
                    &[PromptButton::ok(go), PromptButton::cancel("Stay")],
                    cx,
                )
            }
        };
        cx.spawn(async move |this, cx| {
            // The first button is always the one that says yes.
            let accept = answer.await.ok() == Some(0);
            this.update(cx, |this, cx| {
                this.webview.read(cx).resolve_dialog(id, accept, None);
            })
            .ok();
        })
        .detach();
    }

    fn finish_text_prompt(&mut self, accept: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.text_prompt.take() else {
            return;
        };
        self.webview
            .read(cx)
            .resolve_dialog(prompt.id, accept, accept.then_some(prompt.text));
        let focus = self.webview.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn on_text_prompt_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "enter" => return self.finish_text_prompt(true, window, cx),
            "escape" => return self.finish_text_prompt(false, window, cx),
            _ => {}
        }
        let Some(prompt) = self.text_prompt.as_mut() else {
            return;
        };
        if keystroke.key == "backspace" {
            prompt.text.pop();
        } else if keystroke.modifiers.platform || keystroke.modifiers.control {
            return;
        } else if let Some(input) = keystroke.key_char.as_deref() {
            prompt.text.push_str(input);
        }
        cx.notify();
    }

    /// The page's file chooser gets the native open or save panel.
    fn answer_file_dialog(
        &mut self,
        id: FileDialogId,
        mode: FileDialogMode,
        default_path: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.demo.auto {
            let paths = match mode {
                FileDialogMode::Open | FileDialogMode::OpenMultiple => {
                    vec![self.demo.sample_file()]
                }
                FileDialogMode::OpenFolder => vec![self.demo.dir.clone()],
                FileDialogMode::Save => vec![self.demo.output("saved-by-page.txt")],
            };
            self.webview.read(cx).resolve_file_dialog(id, Some(paths));
            return;
        }

        if mode == FileDialogMode::Save {
            let (directory, name) = split_default_path(default_path.as_deref());
            let chosen = cx.prompt_for_new_path(&directory, name.as_deref());
            cx.spawn(async move |this, cx| {
                let path = chosen.await.ok().and_then(Result::ok).flatten();
                this.update(cx, |this, cx| {
                    this.webview
                        .read(cx)
                        .resolve_file_dialog(id, path.map(|path| vec![path]));
                })
                .ok();
            })
            .detach();
            return;
        }

        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: mode != FileDialogMode::OpenFolder,
            directories: mode == FileDialogMode::OpenFolder,
            multiple: mode == FileDialogMode::OpenMultiple,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let paths = chosen.await.ok().and_then(Result::ok).flatten();
            this.update(cx, |this, cx| {
                this.webview.read(cx).resolve_file_dialog(id, paths);
            })
            .ok();
        })
        .detach();
    }

    /// A download asks where to go with the native save panel.
    fn answer_download(&mut self, id: DownloadId, suggested_name: &str, cx: &mut Context<Self>) {
        if self.demo.auto {
            let path =
                (!demo::asks_to_decline(suggested_name)).then(|| self.demo.output(suggested_name));
            self.webview.read(cx).resolve_download(id, path);
            return;
        }
        let chosen = cx.prompt_for_new_path(&downloads_dir(), Some(suggested_name));
        cx.spawn(async move |this, cx| {
            let path = chosen.await.ok().and_then(Result::ok).flatten();
            this.update(cx, |this, cx| {
                // No path means the panel was cancelled, which cancels the
                // download.
                this.webview.read(cx).resolve_download(id, path);
            })
            .ok();
        })
        .detach();
    }

    fn print_to_pdf(&mut self, cx: &mut Context<Self>) {
        if self.demo.auto {
            let path = self.demo.output("page.pdf");
            if let Err(err) = self
                .webview
                .read(cx)
                .print_to_pdf(path, &PdfOptions::default())
            {
                log::error!("could not print: {err}");
            }
            return;
        }
        let chosen = cx.prompt_for_new_path(&downloads_dir(), Some("page.pdf"));
        cx.spawn(async move |this, cx| {
            let Some(path) = chosen.await.ok().and_then(Result::ok).flatten() else {
                return;
            };
            this.update(cx, |this, cx| {
                let options = PdfOptions {
                    print_background: true,
                    ..PdfOptions::default()
                };
                if let Err(err) = this.webview.read(cx).print_to_pdf(path, &options) {
                    log::error!("could not print: {err}");
                }
            })
            .ok();
        })
        .detach();
    }

    /// Chromium's debug URL that crashes the renderer, to show recovery.
    fn crash_renderer(&mut self, cx: &mut Context<Self>) {
        self.webview.read(cx).load_url("chrome://crash");
    }

    fn reload_after_crash(&mut self, cx: &mut Context<Self>) {
        self.crashed = None;
        self.webview.read(cx).reload();
        cx.notify();
    }

    /// What the address bar should show: the edit buffer while editing, the
    /// current URL otherwise.
    fn address_text(&self, cx: &Context<Self>) -> String {
        self.editing_address
            .clone()
            .unwrap_or_else(|| self.webview.read(cx).url())
    }

    fn navigate(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(url) = normalize_url(url) else {
            // Nothing typed; leave the page alone rather than loading about:blank.
            return;
        };
        self.webview.read(cx).load_url(&url);
        self.editing_address = None;
        // Hand the keyboard back to the page.
        let focus = self.webview.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
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
                window.focus(&focus, cx);
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
                            window.focus(&this.address_focus, cx);
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
            // Without flex_shrink_0 the panel gets squeezed: the webview element
            // reports no intrinsic width, so the flex_1 sibling takes the space.
            .flex_shrink_0()
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
            .child(div().h(px(1.)).bg(rgb(BORDER)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_DIM))
                            .child("Handled by the application"),
                    )
                    .children(self.features_url.clone().map(|url| {
                        panel_action(
                            "features",
                            "Feature test page",
                            cx,
                            move |this, window, cx| this.navigate(&url, window, cx),
                        )
                    }))
                    .child(panel_action("print", "Print to PDF", cx, |this, _, cx| {
                        this.print_to_pdf(cx)
                    }))
                    .child(panel_action(
                        "crash",
                        "Crash the renderer",
                        cx,
                        |this, _, cx| this.crash_renderer(cx),
                    )),
            )
            .child(div().h(px(1.)).bg(rgb(BORDER)))
            .child(
                div()
                    .id("events")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap_1()
                    .overflow_y_scroll()
                    .child(div().text_xs().text_color(rgb(TEXT_DIM)).child("Events"))
                    .children(self.events.iter().rev().map(|line| {
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT))
                            .child(SharedString::from(line.clone()))
                    })),
            )
    }

    /// A strip over the page saying the renderer died, with the way back.
    fn render_crash_banner(&self, status: &str, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .mb_2()
            .rounded_md()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(WARNING))
            .text_sm()
            .child(
                div()
                    .flex_1()
                    .child(format!("The page's renderer went away ({status}).")),
            )
            .child(
                div()
                    .id("reload-after-crash")
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(WARNING))
                    .text_color(rgb(0x101014))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.reload_after_crash(cx)))
                    .child("Reload"),
            )
    }

    /// The overlay that answers `prompt()`, drawn over the page like the
    /// badge — something a child-window webview could not do.
    fn render_text_prompt(&self, prompt: &TextPrompt, cx: &mut Context<Self>) -> impl IntoElement {
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_md()
                .text_sm()
                .cursor_pointer()
                .when(primary, |this| {
                    this.bg(rgb(ACCENT)).text_color(rgb(0x101014))
                })
                .when(!primary, |this| this.border_1().border_color(rgb(BORDER)))
                .child(label)
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000088))
            .occlude()
            .child(
                div()
                    .track_focus(&prompt.focus)
                    .on_key_down(cx.listener(Self::on_text_prompt_key))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .w(px(360.))
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_sm()
                            .child(SharedString::from(prompt.message.clone())),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .h(px(30.))
                            .px_2()
                            .rounded_md()
                            .bg(rgb(BG))
                            .border_1()
                            .border_color(rgb(ACCENT))
                            .text_sm()
                            .child(SharedString::from(prompt.text.clone()))
                            .child(div().w(px(1.)).h(px(16.)).bg(rgb(ACCENT))),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                button("prompt-cancel", "Cancel", false).on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.finish_text_prompt(false, window, cx)
                                    },
                                )),
                            )
                            .child(button("prompt-ok", "OK", true).on_click(cx.listener(
                                |this, _, window, cx| this.finish_text_prompt(true, window, cx),
                            ))),
                    ),
            )
    }
}

/// A side panel button.
fn panel_action(
    id: &'static str,
    label: &'static str,
    cx: &mut Context<BrowserWindow>,
    on_click: impl Fn(&mut BrowserWindow, &mut Window, &mut Context<BrowserWindow>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .text_sm()
        .text_color(rgb(TEXT))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(BORDER)))
        .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
        .child(label)
}

/// Where the save panel for downloads and PDFs starts.
fn downloads_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Downloads"))
        .unwrap_or_else(std::env::temp_dir)
}

/// A save panel's starting folder and file name, from the path the page
/// suggested.
fn split_default_path(path: Option<&std::path::Path>) -> (PathBuf, Option<String>) {
    let Some(path) = path else {
        return (downloads_dir(), None);
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    match path.parent().filter(|parent| parent.is_absolute()) {
        Some(parent) => (parent.to_path_buf(), name),
        None => (downloads_dir(), name),
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
        let crash_banner = self
            .crashed
            .clone()
            .map(|status| self.render_crash_banner(&status, cx).into_any_element());
        let text_prompt = self
            .text_prompt
            .as_ref()
            .map(|prompt| self.render_text_prompt(prompt, cx).into_any_element());

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
                        div()
                            .relative()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .p_3()
                            .children(crash_banner)
                            .child(
                                // Clip the webview to rounded corners and draw a gpui
                                // element over it. A child-view backend can do
                                // neither.
                                div()
                                    .relative()
                                    .flex_1()
                                    .w_full()
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
                                    )
                                    .children(text_prompt),
                            ),
                    )
                    .children(panel),
            )
    }
}

/// Turns what someone typed into a URL to load.
///
/// Returns `None` for input that should not navigate at all, so an accidental
/// Enter on an empty address bar does not blank the page.
fn normalize_url(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Already a URL, including file:// and about:blank.
    if trimmed.contains("://") || trimmed.starts_with("about:") {
        return Some(trimmed.to_string());
    }

    if looks_like_a_host(trimmed) {
        // Local development servers almost never speak https.
        let scheme = if is_loopback(trimmed) {
            "http"
        } else {
            "https"
        };
        return Some(format!("{scheme}://{trimmed}"));
    }

    Some(format!("https://duckduckgo.com/?q={}", urlencode(trimmed)))
}

/// Whether this looks like somewhere to connect to rather than something to
/// search for.
fn looks_like_a_host(input: &str) -> bool {
    if input.contains(char::is_whitespace) {
        return false;
    }

    let host = input
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    let host = host.split(':').next().unwrap_or_default();

    if host.is_empty() {
        return false;
    }
    // "localhost" and "localhost:3000" have no dot but are still hosts.
    if is_loopback(input) {
        return true;
    }
    // Otherwise require a dotted name with a non-empty last label, so "3.5" is a
    // host but "hello." and "1." are searches.
    match host.rsplit_once('.') {
        Some((before, tld)) => !before.is_empty() && !tld.is_empty(),
        None => false,
    }
}

/// Whether the input points at this machine.
fn is_loopback(input: &str) -> bool {
    let host = input
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default();
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "[::1]"
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
    gpui_platform::application().run(move |cx| {
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
    runtime.shutdown();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_address_bar_does_not_navigate() {
        assert_eq!(normalize_url(""), None);
        assert_eq!(normalize_url("   "), None);
    }

    #[test]
    fn existing_schemes_are_left_alone() {
        assert_eq!(
            normalize_url("https://example.com/a?b=c"),
            Some("https://example.com/a?b=c".into())
        );
        assert_eq!(
            normalize_url("file:///tmp/page.html"),
            Some("file:///tmp/page.html".into())
        );
        assert_eq!(normalize_url("about:blank"), Some("about:blank".into()));
    }

    #[test]
    fn bare_hosts_get_https() {
        assert_eq!(
            normalize_url("example.com"),
            Some("https://example.com".into())
        );
        assert_eq!(
            normalize_url("example.com:8080/path"),
            Some("https://example.com:8080/path".into())
        );
        assert_eq!(
            normalize_url("192.168.0.1"),
            Some("https://192.168.0.1".into())
        );
    }

    #[test]
    fn local_development_servers_get_http() {
        // A dev server on localhost almost never has a certificate.
        assert_eq!(
            normalize_url("localhost:3000"),
            Some("http://localhost:3000".into())
        );
        assert_eq!(normalize_url("localhost"), Some("http://localhost".into()));
        assert_eq!(
            normalize_url("127.0.0.1:8080/health"),
            Some("http://127.0.0.1:8080/health".into())
        );
    }

    #[test]
    fn everything_else_is_a_search() {
        assert_eq!(
            normalize_url("hello world"),
            Some("https://duckduckgo.com/?q=hello+world".into())
        );
        // A trailing dot is a typo, not a host.
        assert_eq!(
            normalize_url("hello."),
            Some("https://duckduckgo.com/?q=hello.".into())
        );
        assert_eq!(
            normalize_url("rust"),
            Some("https://duckduckgo.com/?q=rust".into())
        );
    }

    #[test]
    fn search_terms_are_percent_encoded() {
        assert_eq!(urlencode("a b&c"), "a+b%26c");
        // Multi-byte input has to be encoded per UTF-8 byte.
        assert_eq!(urlencode("あ"), "%E3%81%82");
    }
}
