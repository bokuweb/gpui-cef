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

pub use options::{CookieSpec, RuntimeOptions, SameSite, WebviewOptions};
pub use platform::{init, Runtime, Webview};

/// What a [`Webview`] reports to the application, through
/// [`gpui::EventEmitter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebviewEvent {
    /// A message the page posted with the script [`post_message_script`]
    /// returns.
    ///
    /// Untrusted: any script running in the page can post one, so treat it as
    /// input from the page, not from the application.
    Message(String),
}

/// What a console line starts with when it is a message for the application.
/// The page never needs to spell it out: [`post_message_script`] does.
pub const MESSAGE_PREFIX: &str = "gpui-cef:message:";

/// JavaScript that evaluates `expression`, turns it into a string, and posts
/// it to the application as a [`WebviewEvent::Message`]. Pass it to
/// [`Webview::eval`], or embed it in a script that does.
///
/// Messages travel through the page's console, which both backends can
/// observe without code in the page's own process.
pub fn post_message_script(expression: &str) -> String {
    format!("console.debug({MESSAGE_PREFIX:?} + String({expression}))")
}

/// The message a console line carries, if it is one.
pub fn message_payload(line: &str) -> Option<&str> {
    line.strip_prefix(MESSAGE_PREFIX)
}

/// Something went wrong bringing up the browser engine.
#[derive(Debug)]
pub enum Error {
    /// The Chromium Embedded Framework could not be loaded. On macOS the
    /// executable has to live inside a bundled `.app`.
    LibraryLoad(String),
    /// `cef_initialize` failed.
    Initialize,
    /// The backend cannot do this on this platform.
    Unsupported(&'static str),
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
            Error::Unsupported(what) => write!(f, "not supported on this platform: {what}"),
            Error::Backend(msg) => write!(f, "failed to create the webview backend: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

/// The result type used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_posted_message_reads_back_from_its_console_line() {
        let script = post_message_script("document.title");
        assert_eq!(
            script,
            r#"console.debug("gpui-cef:message:" + String(document.title))"#
        );
        assert_eq!(
            message_payload(r#"gpui-cef:message:{"a":1}"#),
            Some(r#"{"a":1}"#)
        );
        assert_eq!(message_payload("an ordinary log line"), None);
    }
}
