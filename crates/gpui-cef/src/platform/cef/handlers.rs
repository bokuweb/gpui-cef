//! The CEF-side callbacks.
//!
//! cef-rs's `wrap_*!` macros define a handler as "a struct with fields, plus
//! overrides of the trait's default methods". The fields have to be `Clone`, so
//! shared state is passed around as `Rc<Shared>`.

use std::rc::Rc;

use cef::{rc::Rc as _, *};
use core_foundation::base::TCFType;
use core_video::pixel_buffer::CVPixelBuffer;
use gpui::{px, size, Bounds, CursorStyle, RenderImage};

use super::shared::{FileDialogReply, Frame as SharedFrame, JsDialogReply, Shared};
use crate::{
    DialogPolicy, DownloadId, DownloadState, FileDialogMode as Mode, JsDialogKind,
    NavigationRequest, PdfMargins, PdfOptions, PopupPolicy, WebviewEvent, WebviewOptions,
};

type Status = crate::TerminationStatus;

/// Settings carried by `cef::App`.
#[derive(Clone)]
pub(crate) struct AppConfig {
    pub(crate) remote_debugging_port: Option<u16>,
    pub(crate) log_to_stderr: bool,
}

wrap_app! {
    pub(crate) struct GpuiCefApp {
        config: AppConfig,
    }

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(command_line) = command_line else {
                return;
            };

            // Off-screen rendering: keep CEF from creating windows of its own.
            command_line.append_switch(Some(&"no-startup-window".into()));
            command_line.append_switch(Some(&"noerrdialogs".into()));
            command_line.append_switch(Some(&"hide-crash-restore-bubble".into()));
            // Avoids the macOS Keychain prompt. Only meaningful in development.
            command_line.append_switch(Some(&"use-mock-keychain".into()));

            // An escape hatch for when the child processes will not start. Not
            // for everyday use: turning the sandbox off removes Chromium's
            // process isolation.
            if std::env::var("GPUI_CEF_NO_SANDBOX").is_ok() {
                command_line.append_switch(Some(&"no-sandbox".into()));
            }

            if self.config.log_to_stderr {
                command_line.append_switch(Some(&"enable-logging=stderr".into()));
            }

            if let Some(port) = self.config.remote_debugging_port {
                command_line.append_switch_with_value(
                    Some(&"remote-debugging-port".into()),
                    Some(&port.to_string().as_str().into()),
                );
            }
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(GpuiCefBrowserProcessHandler::new())
        }
    }
}

wrap_browser_process_handler! {
    pub(crate) struct GpuiCefBrowserProcessHandler;

    impl BrowserProcessHandler {
        /// With `external_message_pump` enabled, CEF uses this to say when it
        /// would like `do_message_loop_work()` to be called.
        ///
        /// Nothing to do here: [`super::pump`] drives the loop from a 60Hz timer
        /// on the main run loop, so the call happens within 16ms regardless.
        /// Calling more often than requested only costs a little efficiency.
        fn on_schedule_message_pump_work(&self, _delay_ms: i64) {}
    }
}

/// The render handler proper. It only stashes frames in `Shared`; gpui does the
/// actual drawing.
#[derive(Clone)]
pub(crate) struct RenderState {
    pub(crate) shared: Rc<Shared>,
}

wrap_render_handler! {
    pub(crate) struct GpuiCefRenderHandler {
        state: RenderState,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            let size = self.state.shared.view_size();
            // CEF does not accept a zero-sized view.
            rect.x = 0;
            rect.y = 0;
            rect.width = (f32::from(size.width).round() as i32).max(1);
            rect.height = (f32::from(size.height).round() as i32).max(1);
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            let Some(screen_info) = screen_info else {
                return 0;
            };
            screen_info.device_scale_factor = self.state.shared.scale_factor();
            1
        }

        /// Where the text being composed sits on screen. macOS needs this to put
        /// the IME candidate window next to the caret rather than in the corner.
        fn on_ime_composition_range_changed(
            &self,
            _browser: Option<&mut Browser>,
            selected_range: Option<&Range>,
            character_bounds: Option<&[Rect]>,
        ) {
            super::input_trace(format_args!(
                "CEF composition range changed selected={selected_range:?} bounds={}",
                character_bounds.map_or(0, <[Rect]>::len)
            ));
            let bounds = character_bounds
                .unwrap_or_default()
                .iter()
                .map(|rect| Bounds {
                    origin: gpui::point(px(rect.x as f32), px(rect.y as f32)),
                    size: size(px(rect.width as f32), px(rect.height as f32)),
                })
                .collect();
            self.state.shared.set_composition_bounds(bounds);
        }

        /// Whether the popup layer — a `<select>` dropdown and the like — is on
        /// screen. CEF draws it as a separate layer, so it has to be composited
        /// over the page rather than ignored.
        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: ::std::os::raw::c_int) {
            self.state.shared.set_popup_visible(show != 0);
        }

        /// Where the popup goes, in the webview's own coordinate space.
        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            let Some(rect) = rect else { return };
            self.state.shared.set_popup_rect(Bounds {
                origin: gpui::point(px(rect.x as f32), px(rect.y as f32)),
                size: size(px(rect.width as f32), px(rect.height as f32)),
            });
        }

        /// The GPU path: take the shared IOSurface CEF drew into and wrap it in a
        /// `CVPixelBuffer`, ready for gpui's `surface()`. No pixels are copied.
        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            let Some(info) = info else { return };
            if info.shared_texture_io_surface.is_null() {
                return;
            }

            // The IOSurface CEF hands over is only guaranteed for the duration of
            // the call, but it is refcounted, so retaining it under the get rule
            // keeps it alive.
            //
            // core-video's CVPixelBuffer::from_io_surface takes the io-surface
            // crate's type, so the deprecation warning is tolerated right here
            // (there is no path through objc2-io-surface).
            #[allow(deprecated)]
            let surface = unsafe {
                io_surface::IOSurface::wrap_under_get_rule(
                    info.shared_texture_io_surface as io_surface::IOSurfaceRef,
                )
            };

            match CVPixelBuffer::from_io_surface(&surface, None) {
                Ok(buffer) => {
                    let frame = SharedFrame::Accelerated(buffer);
                    if type_ == PaintElementType::POPUP {
                        self.state.shared.put_popup_frame(frame);
                    } else {
                        self.state.shared.put_frame(frame);
                    }
                }
                Err(status) => {
                    log::error!("CVPixelBufferCreateWithIOSurface failed: {status}");
                }
            }
        }

        /// The CPU path, for when shared textures are unavailable. Copies BGRA
        /// pixels every frame, so it is slower than the accelerated one.
        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }

            let len = width as usize * height as usize * 4;
            let pixels = unsafe { std::slice::from_raw_parts(buffer, len) }.to_vec();

            // gpui's textures expect BGRA, which is exactly what CEF produces.
            let Some(image) = image::ImageBuffer::from_raw(width as u32, height as u32, pixels)
            else {
                return;
            };
            let frame = SharedFrame::Cpu(std::sync::Arc::new(RenderImage::new(vec![
                image::Frame::new(image),
            ])));
            if type_ == PaintElementType::POPUP {
                self.state.shared.put_popup_frame(frame);
            } else {
                self.state.shared.put_frame(frame);
            }
        }
    }
}

/// Picks up title and URL changes.
#[derive(Clone)]
pub(crate) struct DisplayState {
    pub(crate) shared: Rc<Shared>,
}

wrap_display_handler! {
    pub(crate) struct GpuiCefDisplayHandler {
        state: DisplayState,
    }

    impl DisplayHandler {
        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            if let Some(title) = title {
                self.state.shared.set_title(title.to_string());
            }
        }

        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut cef::Frame>,
            url: Option<&CefString>,
        ) {
            if let Some(url) = url {
                self.state.shared.set_url(url.to_string());
            }
        }

        /// A console line carrying [`crate::MESSAGE_PREFIX`] is a message for
        /// the application: it is queued for the pump and kept off the console.
        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            _level: cef::LogSeverity,
            message: Option<&CefString>,
            _source: Option<&CefString>,
            _line: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let Some(message) = message.map(|message| message.to_string()) else {
                return 0;
            };
            match crate::message_payload(&message) {
                Some(payload) => {
                    self.state
                        .shared
                        .push_event(crate::WebviewEvent::Message(payload.to_string()));
                    1
                }
                None => 0,
            }
        }

        /// Off-screen rendering means CEF never touches the real cursor, so the
        /// page's choice is forwarded to gpui instead.
        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: *mut u8,
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> ::std::os::raw::c_int {
            self.state.shared.set_cursor(cursor_style(type_));
            // Returning 1 tells CEF the cursor has been dealt with.
            1
        }
    }
}

/// Maps CEF's cursor kinds onto the closest thing gpui can show.
fn cursor_style(cursor: CursorType) -> CursorStyle {
    match cursor {
        CursorType::HAND => CursorStyle::PointingHand,
        CursorType::IBEAM => CursorStyle::IBeam,
        CursorType::VERTICALTEXT => CursorStyle::IBeamCursorForVerticalLayout,
        CursorType::CROSS | CursorType::CELL => CursorStyle::Crosshair,
        CursorType::MOVE | CursorType::MIDDLEPANNING => CursorStyle::ClosedHand,
        CursorType::GRAB => CursorStyle::OpenHand,
        CursorType::GRABBING => CursorStyle::ClosedHand,
        CursorType::CONTEXTMENU => CursorStyle::ContextualMenu,
        CursorType::ALIAS | CursorType::DND_LINK => CursorStyle::DragLink,
        CursorType::COPY | CursorType::DND_COPY => CursorStyle::DragCopy,
        CursorType::NOTALLOWED | CursorType::NODROP | CursorType::DND_NONE => {
            CursorStyle::OperationNotAllowed
        }
        // gpui has no hidden cursor; the arrow is the least surprising stand-in.
        CursorType::NONE => CursorStyle::Arrow,
        CursorType::EASTRESIZE | CursorType::WESTRESIZE => CursorStyle::ResizeLeftRight,
        CursorType::NORTHRESIZE | CursorType::SOUTHRESIZE => CursorStyle::ResizeUpDown,
        CursorType::EASTWESTRESIZE => CursorStyle::ResizeLeftRight,
        CursorType::NORTHSOUTHRESIZE => CursorStyle::ResizeUpDown,
        CursorType::NORTHEASTRESIZE | CursorType::SOUTHWESTRESIZE => {
            CursorStyle::ResizeUpRightDownLeft
        }
        CursorType::NORTHWESTRESIZE | CursorType::SOUTHEASTRESIZE => {
            CursorStyle::ResizeUpLeftDownRight
        }
        CursorType::NORTHEASTSOUTHWESTRESIZE => CursorStyle::ResizeUpRightDownLeft,
        CursorType::NORTHWESTSOUTHEASTRESIZE => CursorStyle::ResizeUpLeftDownRight,
        CursorType::COLUMNRESIZE => CursorStyle::ResizeColumn,
        CursorType::ROWRESIZE => CursorStyle::ResizeRow,
        // WAIT, PROGRESS, HELP, ZOOMIN/OUT and the panning cursors have no gpui
        // equivalent; the arrow is the least surprising fallback.
        _ => CursorStyle::Arrow,
    }
}

/// Picks up load progress and whether history navigation is possible.
#[derive(Clone)]
pub(crate) struct LoadState {
    pub(crate) shared: Rc<Shared>,
}

wrap_load_handler! {
    pub(crate) struct GpuiCefLoadHandler {
        state: LoadState,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: ::std::os::raw::c_int,
            can_go_back: ::std::os::raw::c_int,
            can_go_forward: ::std::os::raw::c_int,
        ) {
            self.state.shared.set_load_state(
                is_loading != 0,
                can_go_back != 0,
                can_go_forward != 0,
            );
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut cef::Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            // ERR_ABORTED just means another navigation replaced this one, or
            // the navigation filter turned it down.
            if error_code == Errorcode::ABORTED {
                return;
            }
            let url = string(failed_url);
            let error_text = string(error_text);
            log::warn!("failed to load {url}: {error_text} ({error_code:?})");
            self.state.shared.push_event(WebviewEvent::LoadFailed {
                url,
                error_code: *error_code.as_ref() as i32,
                error_text,
                is_main_frame: frame.is_none_or(|frame| frame.is_main() != 0),
            });
        }
    }
}

/// What the handlers that act on the application's behalf — popups,
/// navigation, dialogs, downloads — need: somewhere to queue events, and the
/// policies the application chose.
#[derive(Clone)]
pub(crate) struct PageState {
    pub(crate) shared: Rc<Shared>,
    pub(crate) options: Rc<WebviewOptions>,
}

impl PageState {
    /// A blocked popup becomes an event; one allowed in place becomes a
    /// navigation of the main frame.
    fn handle_popup(
        &self,
        browser: Option<&mut Browser>,
        url: String,
        target_frame: String,
        user_gesture: bool,
    ) {
        match self.options.popups {
            PopupPolicy::Block => self.shared.push_event(WebviewEvent::PopupRequested {
                url,
                target_frame,
                user_gesture,
            }),
            PopupPolicy::LoadInPlace => {
                // A bare window.open() has nowhere to go.
                if url.is_empty() {
                    return;
                }
                if let Some(frame) = browser.and_then(|browser| browser.main_frame()) {
                    frame.load_url(Some(&url.as_str().into()));
                }
            }
        }
    }
}

wrap_life_span_handler! {
    pub(crate) struct GpuiCefLifeSpanHandler {
        state: PageState,
    }

    impl LifeSpanHandler {
        /// `window.open` and `target="_blank"` links. Always cancelled: Alloy
        /// would otherwise create a native popup window the application has no
        /// hold on. What happens instead is up to [`PopupPolicy`].
        fn on_before_popup(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut cef::Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            self.state.handle_popup(
                browser,
                string(target_url),
                string(target_frame_name),
                user_gesture != 0,
            );
            1
        }
    }
}

wrap_request_handler! {
    pub(crate) struct GpuiCefRequestHandler {
        state: PageState,
    }

    impl RequestHandler {
        /// Every navigation, of the page and of its frames, passes through
        /// [`crate::WebviewOptions::navigation_filter`] here.
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut cef::Frame>,
            request: Option<&mut Request>,
            user_gesture: ::std::os::raw::c_int,
            is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let Some(filter) = &self.state.options.navigation_filter else {
                return 0;
            };
            let navigation = NavigationRequest {
                url: request
                    .map(|request| CefString::from(&request.url()).to_string())
                    .unwrap_or_default(),
                is_redirect: is_redirect != 0,
                user_gesture: user_gesture != 0,
                is_main_frame: frame.is_none_or(|frame| frame.is_main() != 0),
            };
            if crate::options::navigation_allowed(filter, &navigation) {
                return 0;
            }
            self.state
                .shared
                .push_event(WebviewEvent::NavigationBlocked { url: navigation.url });
            // Cancelled; CEF reports ERR_ABORTED to the load handler, which
            // leaves it out.
            1
        }

        /// The renderer died. The page is frozen on its last frame until the
        /// application reloads it, so it has to hear about this.
        fn on_render_process_terminated(
            &self,
            _browser: Option<&mut Browser>,
            status: TerminationStatus,
            error_code: ::std::os::raw::c_int,
            error_string: Option<&CefString>,
        ) {
            log::error!("the render process terminated: {status:?} ({error_code})");
            // Nothing is left to answer them.
            self.state.shared.cancel_js_dialogs();
            self.state
                .shared
                .push_event(WebviewEvent::RenderProcessTerminated {
                    status: termination_status(status),
                    error_code,
                    error_text: string(error_string),
                });
        }

        /// A link the user asked to open somewhere else — Cmd-click or
        /// middle-click. Left alone, Chromium would load it here; it goes
        /// through [`PopupPolicy`] like any other new window instead.
        fn on_open_urlfrom_tab(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut cef::Frame>,
            target_url: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            user_gesture: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            if !opens_new_window(target_disposition) {
                return 0;
            }
            match self.state.options.popups {
                // Letting it through loads it in this browser, which is the
                // policy.
                PopupPolicy::LoadInPlace => 0,
                PopupPolicy::Block => {
                    self.state
                        .handle_popup(browser, string(target_url), String::new(), user_gesture != 0);
                    1
                }
            }
        }
    }
}

wrap_jsdialog_handler! {
    pub(crate) struct GpuiCefJsDialogHandler {
        state: PageState,
    }

    impl JsdialogHandler {
        /// `alert`, `confirm` and `prompt`. A windowless browser has no dialog
        /// of its own, so the application answers — see [`DialogPolicy`].
        fn on_jsdialog(
            &self,
            _browser: Option<&mut Browser>,
            origin_url: Option<&CefString>,
            dialog_type: JsdialogType,
            message_text: Option<&CefString>,
            default_prompt_text: Option<&CefString>,
            callback: Option<&mut JsdialogCallback>,
            suppress_message: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            let (Some(kind), Some(callback)) = (js_dialog_kind(dialog_type), callback) else {
                return 0;
            };
            if self.state.options.js_dialogs == DialogPolicy::AutoDismiss
                || self.state.shared.is_closed()
            {
                // Suppressing, which CEF prefers to answering at once: alert
                // returns, confirm returns false and prompt null.
                if let Some(suppress) = suppress_message {
                    *suppress = 1;
                }
                return 0;
            }
            let id = self.state.shared.add_js_dialog(JsDialogReply(callback.clone()));
            self.state.shared.push_event(WebviewEvent::JsDialog {
                id,
                kind,
                message: string(message_text),
                default_prompt: string(default_prompt_text),
                origin_url: string(origin_url),
            });
            1
        }

        /// The "leave this page?" dialog a `beforeunload` handler asks for.
        fn on_before_unload_dialog(
            &self,
            _browser: Option<&mut Browser>,
            message_text: Option<&CefString>,
            is_reload: ::std::os::raw::c_int,
            callback: Option<&mut JsdialogCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(callback) = callback else {
                return 0;
            };
            if self.state.options.js_dialogs == DialogPolicy::AutoDismiss
                || self.state.shared.is_closed()
            {
                // Leave: staying would make the navigation hang on a question
                // nobody is going to answer.
                callback.cont(1, None);
                return 1;
            }
            let id = self.state.shared.add_js_dialog(JsDialogReply(callback.clone()));
            self.state.shared.push_event(WebviewEvent::JsDialog {
                id,
                kind: JsDialogKind::BeforeUnload {
                    is_reload: is_reload != 0,
                },
                message: string(message_text),
                default_prompt: String::new(),
                origin_url: String::new(),
            });
            1
        }

        /// The page moved on — navigated, say — so a dialog still waiting no
        /// longer means anything.
        fn on_reset_dialog_state(&self, _browser: Option<&mut Browser>) {
            self.state.shared.cancel_js_dialogs();
        }
    }
}

wrap_dialog_handler! {
    pub(crate) struct GpuiCefDialogHandler {
        state: PageState,
    }

    impl DialogHandler {
        /// `<input type="file">` and the file picker APIs. CEF's own picker
        /// needs a window, so the application chooses instead.
        fn on_file_dialog(
            &self,
            _browser: Option<&mut Browser>,
            mode: FileDialogMode,
            title: Option<&CefString>,
            default_file_path: Option<&CefString>,
            accept_filters: Option<&mut CefStringList>,
            accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(callback) = callback else {
                return 0;
            };
            let (Some(mode), false) = (file_dialog_mode(mode), self.state.shared.is_closed())
            else {
                callback.cancel();
                return 1;
            };
            let filters = string_list(accept_filters);
            let extensions = accept_extensions_of(&filters, &string_list(accept_extensions));
            let id = self.state.shared.add_file_dialog(FileDialogReply(callback.clone()));
            self.state.shared.push_event(WebviewEvent::FileDialog {
                id,
                mode,
                title: string(title),
                default_path: Some(string(default_file_path))
                    .filter(|path| !path.is_empty())
                    .map(Into::into),
                accept_filters: filters,
                accept_extensions: extensions,
            });
            // Returning 1 keeps CEF from asking again or showing a picker of
            // its own.
            1
        }
    }
}

wrap_download_handler! {
    pub(crate) struct GpuiCefDownloadHandler {
        state: PageState,
    }

    impl DownloadHandler {
        /// Lets every download through to `on_before_download`, which is
        /// where the application decides. CEF's own default says yes too,
        /// but a method left out of a cef-rs handler answers 0 — no.
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            _url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> ::std::os::raw::c_int {
            1
        }

        /// A download is starting and CEF wants to know where it goes. The
        /// application says, through `Webview::resolve_download`.
        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> ::std::os::raw::c_int {
            let (Some(item), Some(callback)) = (download_item, callback) else {
                return 0;
            };
            // CEF says not to keep the item, so read what is needed now.
            let id = item.id();
            if !self.state.shared.add_download(id, callback.clone()) {
                return 1;
            }
            self.state.shared.push_event(WebviewEvent::DownloadRequested {
                id: DownloadId(id.into()),
                url: CefString::from(&item.url()).to_string(),
                suggested_name: string(suggested_name),
                mime_type: CefString::from(&item.mime_type()).to_string(),
                total_bytes: byte_count(item.total_bytes()),
            });
            // Returning 1 means the callback will be called, later.
            1
        }

        fn on_download_updated(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            callback: Option<&mut DownloadItemCallback>,
        ) {
            let (Some(item), Some(callback)) = (download_item, callback) else {
                return;
            };
            let state = download_state(
                item.is_complete() != 0,
                item.is_canceled() != 0,
                item.is_interrupted() != 0,
            );
            let finished = matches!(state, DownloadState::Completed | DownloadState::Canceled);
            let id = item.id();
            if !self.state.shared.update_download(id, callback.clone(), finished) {
                return;
            }
            let path = CefString::from(&item.full_path()).to_string();
            self.state.shared.push_event(WebviewEvent::DownloadUpdated {
                id: DownloadId(id.into()),
                state,
                received_bytes: byte_count(item.received_bytes()).unwrap_or(0),
                total_bytes: byte_count(item.total_bytes()),
                path: Some(path).filter(|path| !path.is_empty()).map(Into::into),
            });
        }
    }
}

/// Where a download stands, from CEF's flags. An interrupted download may
/// still resume, so it is not taken as finished.
fn download_state(complete: bool, canceled: bool, interrupted: bool) -> DownloadState {
    if complete {
        DownloadState::Completed
    } else if canceled {
        DownloadState::Canceled
    } else if interrupted {
        DownloadState::Failed
    } else {
        DownloadState::InProgress
    }
}

/// A byte count CEF reports, which is negative or zero when unknown.
fn byte_count(bytes: i64) -> Option<u64> {
    u64::try_from(bytes).ok().filter(|bytes| *bytes > 0)
}

fn file_dialog_mode(mode: FileDialogMode) -> Option<Mode> {
    match mode {
        FileDialogMode::OPEN => Some(Mode::Open),
        FileDialogMode::OPEN_MULTIPLE => Some(Mode::OpenMultiple),
        FileDialogMode::OPEN_FOLDER => Some(Mode::OpenFolder),
        FileDialogMode::SAVE => Some(Mode::Save),
        _ => None,
    }
}

/// The file extensions an `accept` attribute allows, lower-case and without
/// the dot. CEF lists the filters as written (`image/*`, `.DOCX`) and, beside
/// each, the extensions a MIME type stands for (`.png;.jpg`), or nothing when
/// it does not know.
fn accept_extensions_of(filters: &[String], expansions: &[String]) -> Vec<String> {
    let mut extensions: Vec<String> = Vec::new();
    for (index, filter) in filters.iter().enumerate() {
        let expansion = expansions
            .get(index)
            .map(String::as_str)
            .unwrap_or_default();
        let candidates = if expansion.is_empty() && filter.starts_with('.') {
            filter.as_str()
        } else {
            expansion
        };
        for extension in candidates.split(';') {
            let extension = extension.trim().trim_start_matches('.').to_lowercase();
            if !extension.is_empty() && !extensions.contains(&extension) {
                extensions.push(extension);
            }
        }
    }
    extensions
}

/// The strings in a list CEF passed to a callback. It still owns the list.
fn string_list(list: Option<&mut CefStringList>) -> Vec<String> {
    let Some(list) = list else {
        return Vec::new();
    };
    let raw: *mut cef::sys::_cef_string_list_t = list.into();
    if raw.is_null() {
        return Vec::new();
    }
    let count = unsafe { cef::sys::cef_string_list_size(raw) };
    (0..count)
        .filter_map(|index| unsafe {
            let mut value: cef::sys::cef_string_t = std::mem::zeroed();
            if cef::sys::cef_string_list_value(raw, index, &mut value) == 0 {
                return None;
            }
            let text = CefString::from(std::ptr::from_ref(&value)).to_string();
            // The list hands out a copy, which is ours to free.
            cef::sys::cef_string_utf16_clear(&mut value);
            Some(text)
        })
        .collect()
}

fn js_dialog_kind(dialog_type: JsdialogType) -> Option<JsDialogKind> {
    match dialog_type {
        JsdialogType::ALERT => Some(JsDialogKind::Alert),
        JsdialogType::CONFIRM => Some(JsDialogKind::Confirm),
        JsdialogType::PROMPT => Some(JsDialogKind::Prompt),
        _ => None,
    }
}

wrap_pdf_print_callback! {
    pub(crate) struct GpuiCefPdfPrintCallback {
        shared: Rc<Shared>,
    }

    impl PdfPrintCallback {
        fn on_pdf_print_finished(&self, path: Option<&CefString>, ok: ::std::os::raw::c_int) {
            self.shared.push_event(WebviewEvent::PdfPrinted {
                path: string(path).into(),
                ok: ok != 0,
            });
        }
    }
}

/// [`PdfOptions`] in CEF's shape. Sizes are in inches on both sides.
pub(crate) fn pdf_settings(options: &PdfOptions) -> PdfPrintSettings {
    let (width, height) = options.paper_size.unwrap_or_default();
    let (margin_type, [top, right, bottom, left]) = match options.margins {
        PdfMargins::Default => (PdfPrintMarginType::DEFAULT, [0.; 4]),
        PdfMargins::None => (PdfPrintMarginType::NONE, [0.; 4]),
        PdfMargins::Custom {
            top,
            right,
            bottom,
            left,
        } => (PdfPrintMarginType::CUSTOM, [top, right, bottom, left]),
    };
    let mut settings = PdfPrintSettings {
        landscape: options.landscape.into(),
        print_background: options.print_background.into(),
        // Zero means "the default", for the paper as for the scale.
        paper_width: width,
        paper_height: height,
        prefer_css_page_size: options.prefer_css_page_size.into(),
        margin_type,
        margin_top: top,
        margin_right: right,
        margin_bottom: bottom,
        margin_left: left,
        ..Default::default()
    };
    if let Some(ranges) = &options.page_ranges {
        settings.page_ranges = ranges.as_str().into();
    }
    settings
}

fn termination_status(status: TerminationStatus) -> Status {
    match status {
        TerminationStatus::PROCESS_WAS_KILLED => Status::Killed,
        TerminationStatus::PROCESS_CRASHED => Status::Crashed,
        TerminationStatus::PROCESS_OOM => Status::OutOfMemory,
        TerminationStatus::LAUNCH_FAILED => Status::LaunchFailed,
        TerminationStatus::INTEGRITY_FAILURE => Status::IntegrityFailure,
        _ => Status::Abnormal,
    }
}

/// Whether a navigation was meant for a new tab or window rather than this
/// one.
fn opens_new_window(disposition: WindowOpenDisposition) -> bool {
    !matches!(
        disposition,
        WindowOpenDisposition::CURRENT_TAB
            | WindowOpenDisposition::SAVE_TO_DISK
            | WindowOpenDisposition::IGNORE_ACTION
            | WindowOpenDisposition::UNKNOWN
    )
}

/// An optional CEF string as an owned one, empty when CEF passed none.
fn string(value: Option<&CefString>) -> String {
    value.map(ToString::to_string).unwrap_or_default()
}

wrap_keyboard_handler! {
    pub(crate) struct GpuiCefKeyboardHandler {}

    impl KeyboardHandler {
        fn on_key_event(
            &self,
            browser: Option<&mut Browser>,
            event: Option<&KeyEvent>,
            _os_event: *mut u8,
        ) -> i32 {
            use super::input::{clipboard_command, ClipboardCommand};

            let Some(event) = event else {
                return 0;
            };
            if let Some(command) = clipboard_command(event) {
                if let Some(frame) = browser.and_then(|browser| browser.focused_frame()) {
                    match command {
                        ClipboardCommand::Copy => frame.copy(),
                        ClipboardCommand::Cut => frame.cut(),
                        ClipboardCommand::Paste => frame.paste(),
                    }
                }
            }
            // OnKeyEvent runs after the renderer and JavaScript have had their
            // chance. GPUI already owns AppKit's initial key/menu dispatch;
            // returning false here sends even ordinary typing back through
            // NSMenu.performKeyEquivalent, including menu validation/actions.
            // Consume only this final native fallback, not the page's input.
            1
        }
    }
}

// Return None for unrelated permissions so their existing CEF behavior remains
// unchanged. Never approve a combined request: ACCEPT applies to every bit.
fn clipboard_permission_result(
    options: &WebviewOptions,
    origin: &str,
    permissions: u32,
) -> Option<PermissionRequestResult> {
    let clipboard = PermissionRequestTypes::CLIPBOARD.get_raw();
    if permissions & clipboard == 0 {
        return None;
    }
    let allowed = permissions == clipboard
        && crate::options::clipboard_access_allowed(options.clipboard_access.as_ref(), origin);
    Some(if allowed {
        PermissionRequestResult::ACCEPT
    } else {
        PermissionRequestResult::DENY
    })
}

wrap_permission_handler! {
    pub(crate) struct GpuiCefPermissionHandler {
        state: PageState,
    }

    impl PermissionHandler {
        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            _prompt_id: u64,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> i32 {
            let Some(callback) = callback else { return 0; };
            let Some(result) = clipboard_permission_result(
                &self.state.options, &string(requesting_origin), requested_permissions,
            ) else { return 0; };
            callback.cont(result);
            1
        }
    }
}

/// The handlers that act on the application's behalf, built from one
/// [`PageState`].
#[derive(Clone)]
pub(crate) struct PageHandlers {
    life_span: LifeSpanHandler,
    request: RequestHandler,
    js_dialog: JsdialogHandler,
    dialog: DialogHandler,
    download: DownloadHandler,
    permission: PermissionHandler,
}

impl PageHandlers {
    fn new(page: PageState) -> Self {
        Self {
            life_span: GpuiCefLifeSpanHandler::new(page.clone()),
            request: GpuiCefRequestHandler::new(page.clone()),
            js_dialog: GpuiCefJsDialogHandler::new(page.clone()),
            dialog: GpuiCefDialogHandler::new(page.clone()),
            permission: GpuiCefPermissionHandler::new(page.clone()),
            download: GpuiCefDownloadHandler::new(page),
        }
    }
}

wrap_client! {
    pub(crate) struct GpuiCefClient {
        render_handler: RenderHandler,
        display_handler: DisplayHandler,
        load_handler: LoadHandler,
        page: PageHandlers,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render_handler.clone())
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display_handler.clone())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load_handler.clone())
        }

        fn keyboard_handler(&self) -> Option<KeyboardHandler> {
            Some(GpuiCefKeyboardHandler::new())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.page.life_span.clone())
        }

        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.page.request.clone())
        }

        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(self.page.js_dialog.clone())
        }

        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(self.page.dialog.clone())
        }

        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(self.page.permission.clone())
        }

        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(self.page.download.clone())
        }
    }
}

impl GpuiCefClient {
    pub(crate) fn build(shared: Rc<Shared>, options: &WebviewOptions) -> Client {
        let page = PageState {
            shared: shared.clone(),
            options: Rc::new(options.clone()),
        };
        Self::new(
            GpuiCefRenderHandler::new(RenderState {
                shared: shared.clone(),
            }),
            GpuiCefDisplayHandler::new(DisplayState {
                shared: shared.clone(),
            }),
            GpuiCefLoadHandler::new(LoadState { shared }),
            PageHandlers::new(page),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_keyboard_fallback_does_not_reenter_the_native_menu() {
        let handler = GpuiCefKeyboardHandler::new();
        // CEF invokes this callback after the renderer. Returning false here
        // invokes NSMenu.performKeyEquivalent again, even for plain typing.
        for key in [
            0x41, 0x31, 0x08, 0x09, 0x0D, 0x1B, 0x25, 0x43, 0x58, 0x56, 0x53,
        ] {
            for type_ in [
                KeyEventType::RAWKEYDOWN,
                KeyEventType::KEYUP,
                KeyEventType::CHAR,
            ] {
                for combination in 0..16 {
                    let modifiers = [1 << 1, 1 << 2, 1 << 3, 1 << 7]
                        .into_iter()
                        .enumerate()
                        .filter(|(index, _)| combination & (1 << index) != 0)
                        .fold(0, |flags, (_, modifier)| flags | modifier);
                    let event = KeyEvent {
                        type_,
                        modifiers,
                        windows_key_code: key,
                        ..Default::default()
                    };
                    assert_eq!(
                        handler.on_key_event(None, Some(&event), std::ptr::null_mut()),
                        1,
                        "CEF must not replay {type_:?}, key {key}, flags {modifiers} to AppKit"
                    );
                }
            }
        }
    }

    #[test]
    fn keyboard_fallback_leaves_the_renderer_first_chance_to_handle_input() {
        let handler = GpuiCefKeyboardHandler::new();
        let event = KeyEvent {
            type_: KeyEventType::RAWKEYDOWN,
            windows_key_code: 0x41,
            ..Default::default()
        };
        assert_eq!(
            handler.on_pre_key_event(None, Some(&event), std::ptr::null_mut(), None),
            0
        );
        assert_eq!(handler.on_key_event(None, None, std::ptr::null_mut()), 0);
    }

    #[test]
    fn clipboard_permissions_deny_by_default_and_only_accept_the_approved_origin() {
        let mut options = WebviewOptions::default();
        let clipboard = PermissionRequestTypes::CLIPBOARD.get_raw();
        assert_eq!(
            clipboard_permission_result(&options, "https://editor.example", clipboard),
            Some(PermissionRequestResult::DENY)
        );
        options.clipboard_access = Some(std::sync::Arc::new(|origin| {
            origin == "https://editor.example"
        }));
        assert_eq!(
            clipboard_permission_result(&options, "https://editor.example", clipboard),
            Some(PermissionRequestResult::ACCEPT)
        );
        for origin in ["", "null", "https://other.example"] {
            assert_eq!(
                clipboard_permission_result(&options, origin, clipboard),
                Some(PermissionRequestResult::DENY)
            );
        }
    }

    #[test]
    fn clipboard_does_not_grant_other_permission_bits_or_call_the_filter_for_them() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let options = WebviewOptions {
            clipboard_access: Some(std::sync::Arc::new(move |_| {
                seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            })),
            ..Default::default()
        };
        let clipboard = PermissionRequestTypes::CLIPBOARD.get_raw();
        assert_eq!(
            clipboard_permission_result(&options, "https://editor.example", 0),
            None
        );
        for bit in 0..32 {
            let other = 1u32 << bit;
            if other == clipboard {
                continue;
            }
            assert_eq!(
                clipboard_permission_result(&options, "https://editor.example", other),
                None
            );
            assert_eq!(
                clipboard_permission_result(&options, "https://editor.example", clipboard | other),
                Some(PermissionRequestResult::DENY)
            );
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn clients_install_the_permission_handler_even_for_default_denials() {
        let client = GpuiCefClient::build(
            Shared::new("about:blank".into()),
            &WebviewOptions::default(),
        );
        let handler = client
            .permission_handler()
            .expect("clipboard requests must get an answer");
        // Without a CEF callback, the request cannot be completed here.
        assert_eq!(
            handler.on_show_permission_prompt(
                None,
                1,
                None,
                PermissionRequestTypes::CLIPBOARD.get_raw(),
                None
            ),
            0
        );
    }

    #[test]
    fn common_cursors_are_mapped() {
        assert_eq!(cursor_style(CursorType::HAND), CursorStyle::PointingHand);
        assert_eq!(cursor_style(CursorType::IBEAM), CursorStyle::IBeam);
        assert_eq!(cursor_style(CursorType::POINTER), CursorStyle::Arrow);
    }

    #[test]
    fn unmapped_cursors_fall_back_to_the_arrow() {
        // No gpui equivalent, and guessing would be worse than an arrow.
        assert_eq!(cursor_style(CursorType::WAIT), CursorStyle::Arrow);
        assert_eq!(cursor_style(CursorType::ZOOMIN), CursorStyle::Arrow);
    }

    #[test]
    fn opposite_resize_edges_share_a_cursor() {
        // Left and right edges look the same, as they do everywhere else on macOS.
        assert_eq!(
            cursor_style(CursorType::EASTRESIZE),
            cursor_style(CursorType::WESTRESIZE)
        );
        assert_eq!(
            cursor_style(CursorType::NORTHRESIZE),
            cursor_style(CursorType::SOUTHRESIZE)
        );
    }

    #[test]
    fn javascript_dialogs_map_to_their_kinds() {
        assert_eq!(
            js_dialog_kind(JsdialogType::ALERT),
            Some(JsDialogKind::Alert)
        );
        assert_eq!(
            js_dialog_kind(JsdialogType::CONFIRM),
            Some(JsDialogKind::Confirm)
        );
        assert_eq!(
            js_dialog_kind(JsdialogType::PROMPT),
            Some(JsDialogKind::Prompt)
        );
    }

    #[test]
    fn file_dialog_modes_map_across() {
        assert_eq!(file_dialog_mode(FileDialogMode::OPEN), Some(Mode::Open));
        assert_eq!(
            file_dialog_mode(FileDialogMode::OPEN_MULTIPLE),
            Some(Mode::OpenMultiple)
        );
        assert_eq!(
            file_dialog_mode(FileDialogMode::OPEN_FOLDER),
            Some(Mode::OpenFolder)
        );
        assert_eq!(file_dialog_mode(FileDialogMode::SAVE), Some(Mode::Save));
    }

    #[test]
    fn accept_filters_become_extensions() {
        let strings = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // A MIME type contributes what CEF expanded it to; an extension
        // stands for itself even when CEF left its expansion empty.
        assert_eq!(
            accept_extensions_of(
                &strings(&["image/*", ".DOCX", "application/x-unknown"]),
                &strings(&[".png;.jpg; .JPG", "", ""])
            ),
            ["png", "jpg", "docx"]
        );
        // Repeats collapse, and no filters means no restriction.
        assert_eq!(
            accept_extensions_of(&strings(&[".txt", ".txt"]), &strings(&[".txt", ""])),
            ["txt"]
        );
        assert!(accept_extensions_of(&[], &[]).is_empty());
    }

    #[test]
    fn download_states_follow_cefs_flags() {
        assert_eq!(
            download_state(false, false, false),
            DownloadState::InProgress
        );
        assert_eq!(download_state(true, false, false), DownloadState::Completed);
        assert_eq!(download_state(false, true, false), DownloadState::Canceled);
        assert_eq!(download_state(false, false, true), DownloadState::Failed);
        // Cancelling interrupts too; cancelled is what it is.
        assert_eq!(download_state(false, true, true), DownloadState::Canceled);
    }

    #[test]
    fn unknown_sizes_are_none() {
        assert_eq!(byte_count(1024), Some(1024));
        assert_eq!(byte_count(0), None);
        assert_eq!(byte_count(-1), None);
    }

    #[test]
    fn termination_statuses_map_across() {
        assert_eq!(
            termination_status(TerminationStatus::PROCESS_CRASHED),
            Status::Crashed
        );
        assert_eq!(
            termination_status(TerminationStatus::PROCESS_WAS_KILLED),
            Status::Killed
        );
        assert_eq!(
            termination_status(TerminationStatus::PROCESS_OOM),
            Status::OutOfMemory
        );
        assert_eq!(
            termination_status(TerminationStatus::ABNORMAL_TERMINATION),
            Status::Abnormal
        );
    }

    #[test]
    fn only_new_tabs_and_windows_count_as_popups() {
        for disposition in [
            WindowOpenDisposition::NEW_FOREGROUND_TAB,
            WindowOpenDisposition::NEW_BACKGROUND_TAB,
            WindowOpenDisposition::NEW_POPUP,
            WindowOpenDisposition::NEW_WINDOW,
        ] {
            assert!(opens_new_window(disposition), "{disposition:?}");
        }
        // Loading here, saving a link, and nothing at all are not windows.
        for disposition in [
            WindowOpenDisposition::CURRENT_TAB,
            WindowOpenDisposition::SAVE_TO_DISK,
            WindowOpenDisposition::IGNORE_ACTION,
        ] {
            assert!(!opens_new_window(disposition), "{disposition:?}");
        }
    }
}
