//! Stub for platforms other than macOS and Windows. It builds, but no webview
//! is shown.
//!
//! Linux should work on the CEF side, so pointing the `#[path]` in [`super`] at
//! the cef backend would probably be enough. That is untested, hence this stub.

use gpui::{div, App, Context, FocusHandle, IntoElement, ParentElement, Render, Styled, Window};

use crate::{RuntimeOptions, WebviewOptions};

/// Holds nothing. Present to match the other backends' API.
#[derive(Clone)]
pub struct Runtime;

impl Runtime {
    /// Does nothing.
    pub fn start(&self, _cx: &mut App) -> crate::Result<()> {
        Ok(())
    }
}

/// Does nothing.
pub fn init(_options: RuntimeOptions) -> crate::Result<Runtime> {
    Ok(Runtime)
}

/// A webview that draws nothing, with the same API as the real backends.
pub struct Webview {
    options: WebviewOptions,
    focus_handle: FocusHandle,
}

impl Webview {
    /// Creates it (and does nothing else).
    pub fn new(_window: &mut Window, cx: &mut Context<Self>, options: WebviewOptions) -> Self {
        Self {
            options,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Navigates to another URL (no-op).
    pub fn load_url(&self, _url: &str) {}
    /// Reloads the current page (no-op).
    pub fn reload(&self) {}
    /// Goes back in history (no-op).
    pub fn go_back(&self) {}
    /// Goes forward in history (no-op).
    pub fn go_forward(&self) {}
    /// Stops the current load (no-op).
    pub fn stop(&self) {}
    /// Runs JavaScript in the page (no-op).
    pub fn eval(&self, _script: &str) {}
    /// The current page title. Always empty.
    pub fn title(&self) -> String {
        String::new()
    }
    /// The current URL.
    pub fn url(&self) -> String {
        self.options.url.clone()
    }
    /// Whether a page is loading. Always false.
    pub fn is_loading(&self) -> bool {
        false
    }
    /// Whether there is history to go back to. Always false.
    pub fn can_go_back(&self) -> bool {
        false
    }
    /// Whether there is history to go forward to. Always false.
    pub fn can_go_forward(&self) -> bool {
        false
    }
}

impl gpui::Focusable for Webview {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Webview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(format!(
            "webview unsupported on this platform ({})",
            self.options.url
        ))
    }
}
