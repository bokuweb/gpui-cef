//! The CEF-side callbacks.
//!
//! cef-rs's `wrap_*!` macros define a handler as "a struct with fields, plus
//! overrides of the trait's default methods". The fields have to be `Clone`, so
//! shared state is passed around as `Rc<Shared>`.

use std::rc::Rc;

use cef::{rc::Rc as _, *};
use core_foundation::base::TCFType;
use core_video::pixel_buffer::CVPixelBuffer;
use gpui::RenderImage;

use super::shared::{Frame as SharedFrame, Shared};

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

        /// The GPU path: take the shared IOSurface CEF drew into and wrap it in a
        /// `CVPixelBuffer`, ready for gpui's `surface()`. No pixels are copied.
        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            // Popups (a <select> dropdown, say) are not drawn yet.
            if type_ != PaintElementType::default() {
                return;
            }
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
                Ok(buffer) => self
                    .state
                    .shared
                    .put_frame(SharedFrame::Accelerated(buffer)),
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
            if type_ != PaintElementType::default() {
                return;
            }
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
            let frame = image::Frame::new(image);
            self.state
                .shared
                .put_frame(SharedFrame::Cpu(std::sync::Arc::new(RenderImage::new(
                    vec![frame],
                ))));
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
            _frame: Option<&mut cef::Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            // ERR_ABORTED just means another navigation replaced this one.
            if error_code == Errorcode::from(cef::sys::cef_errorcode_t::ERR_ABORTED) {
                return;
            }
            log::warn!(
                "failed to load {}: {} ({:?})",
                failed_url.map(|url| url.to_string()).unwrap_or_default(),
                error_text.map(|text| text.to_string()).unwrap_or_default(),
                error_code,
            );
        }
    }
}

wrap_client! {
    pub(crate) struct GpuiCefClient {
        render_handler: RenderHandler,
        display_handler: DisplayHandler,
        load_handler: LoadHandler,
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
    }
}

impl GpuiCefClient {
    pub(crate) fn build(shared: Rc<Shared>) -> Client {
        Self::new(
            GpuiCefRenderHandler::new(RenderState {
                shared: shared.clone(),
            }),
            GpuiCefDisplayHandler::new(DisplayState {
                shared: shared.clone(),
            }),
            GpuiCefLoadHandler::new(LoadState { shared }),
        )
    }
}
