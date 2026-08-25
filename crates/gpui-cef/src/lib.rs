//! Embed a real browser engine as a [gpui] element.
//!
//! The backend is chosen per platform:
//!
//! | OS      | Engine                        | How it is composited |
//! |---------|-------------------------------|----------------------|
//! | macOS   | Chromium (CEF)                | Rendered off-screen and drawn as an ordinary gpui element, so it takes part in gpui's paint order: it can be clipped, rounded, and have other elements drawn on top of it. |
//! | Windows | WebView2 (wry)                | A native child window parented to the gpui window and kept in sync with the laid-out rect. Being a child HWND, it is **always in front of** everything gpui draws. |
//!
//! Why Chromium on macOS: `WKWebView` can only be attached as a child view, so it
//! never becomes part of gpui's scene — no rounded clipping, nothing on top of
//! it. CEF supports off-screen rendering, which makes the page just another
//! texture gpui can composite. WebView2 has no public off-screen API, so the
//! Windows backend uses the straightforward child-window approach.
//!
//! # Usage
//!
//! On macOS, CEF has to be brought up around gpui rather than inside it — see
//! [`init`] and [`Runtime::start`].
//!
//! ```no_run
//! fn main() {
//!     // 1. At the top of main: load CEF into the process.
//!     let runtime = gpui_cef::init(gpui_cef::RuntimeOptions::default()).unwrap();
//!
//!     // Hand the closure a *clone*. Moving the value itself in would run
//!     // cef_shutdown() the moment the closure returns.
//!     let cef = runtime.clone();
//!     gpui::Application::new().run(move |cx| {
//!         // 2. Once gpui has created NSApp, initialize CEF.
//!         cef.start(cx).unwrap();
//!         // ... cx.open_window(..., |window, cx| cx.new(|cx| Webview::new(...)))
//!     });
//!
//!     // 3. The message loop is done, so shut CEF down.
//!     runtime.shutdown();
//! }
//! ```

// The usage example above is deliberately a full `fn main`, since the ordering
// around it is the whole point.
#![allow(clippy::needless_doctest_main)]

mod options;
mod platform;

pub use options::{RuntimeOptions, WebviewOptions};
pub use platform::{init, Runtime, Webview};

/// Something went wrong bringing up the browser engine.
#[derive(Debug)]
pub enum Error {
    /// The Chromium Embedded Framework could not be loaded. On macOS the
    /// executable has to live inside a bundled `.app`.
    LibraryLoad(String),
    /// `cef_initialize` failed.
    Initialize,
    /// The webview backend could not be created.
    Backend(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::LibraryLoad(path) => write!(
                f,
                "failed to load the Chromium Embedded Framework (looked next to {path}). \
                 the executable must live inside a bundled .app — see `just bundle`"
            ),
            Error::Initialize => write!(f, "cef_initialize() failed"),
            Error::Backend(msg) => write!(f, "failed to create the webview backend: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

/// The result type used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;
