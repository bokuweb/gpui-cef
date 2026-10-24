/// Process-wide browser runtime settings.
#[derive(Clone, Debug)]
pub struct RuntimeOptions {
    /// `--remote-debugging-port`. When set, Chrome DevTools can attach at
    /// `http://127.0.0.1:<port>`. macOS (CEF) only.
    pub remote_debugging_port: Option<u16>,
    /// Where CEF keeps its cache. `None` means in-memory / throwaway.
    pub cache_path: Option<std::path::PathBuf>,
    /// Log to stderr.
    pub log_to_stderr: bool,
    /// Where CEF writes its log file. Chromium CHECK failures land here, which
    /// makes this the first place to look when the process dies without a
    /// Rust-side panic.
    pub log_file: Option<std::path::PathBuf>,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            remote_debugging_port: None,
            cache_path: None,
            log_to_stderr: cfg!(debug_assertions),
            log_file: None,
        }
    }
}

/// Settings for a single webview.
#[derive(Clone)]
pub struct WebviewOptions {
    /// The URL to open first.
    pub url: String,
    /// Make the page background transparent, so gpui shows through.
    pub transparent: bool,
    /// Upper bound on the off-screen frame rate. macOS (CEF) only.
    pub frame_rate: i32,
    /// Receive frames through a shared GPU texture (IOSurface). macOS (CEF) only.
    ///
    /// **Off by default, because stock gpui cannot draw these frames.**
    /// gpui's `surface()` hard-codes NV12
    /// (`kCVPixelFormatType_420YpCbCr8BiPlanarFullRange`) and draws the Y and
    /// CbCr planes through a YUV-to-RGB shader — it exists for Zed's screen
    /// sharing. CEF hands out BGRA, so passing it straight through trips an
    /// `assert_eq!` in `metal_renderer.rs`.
    ///
    /// Adding a BGRA branch to gpui closes the gap and makes this path
    /// zero-copy; everything up to that point (CEF to IOSurface to
    /// `CVPixelBuffer` to gpui) is verified working. Only set this to `true`
    /// when running against a gpui that has such a branch.
    ///
    /// When `false`, CEF drives itself at `frame_rate` and hands over BGRA that
    /// is copied on the CPU — roughly 3.9MB per frame at 1200x820.
    pub accelerated: bool,
    /// What to do when the page asks for a new window. Blocked by default, with
    /// a [`crate::WebviewEvent::PopupRequested`] for the application.
    pub popups: PopupPolicy,
    /// Decides whether each navigation may go ahead. `None`, the default,
    /// allows everything.
    pub navigation_filter: Option<NavigationFilter>,
    /// What to do with `alert`, `confirm`, `prompt` and "leave this page?"
    /// dialogs. By default each one becomes a [`crate::WebviewEvent::JsDialog`]
    /// for the application to answer. macOS (CEF) only: WebView2 shows its own.
    pub js_dialogs: DialogPolicy,
    /// Decide whether an origin may use the asynchronous Clipboard API.
    /// Denied by default. macOS (CEF) only.
    ///
    /// Approving an origin allows reading its system clipboard; only approve
    /// trusted application content. Chromium may remember the decision for
    /// the lifetime of the browser context. Native Cmd+C/X/V do not need this.
    pub clipboard_access: Option<ClipboardAccessFilter>,
}

impl Default for WebviewOptions {
    fn default() -> Self {
        Self {
            url: "about:blank".into(),
            transparent: false,
            frame_rate: 60,
            accelerated: false,
            popups: PopupPolicy::default(),
            navigation_filter: None,
            js_dialogs: DialogPolicy::default(),
            clipboard_access: None,
        }
    }
}

impl std::fmt::Debug for WebviewOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebviewOptions")
            .field("url", &self.url)
            .field("transparent", &self.transparent)
            .field("frame_rate", &self.frame_rate)
            .field("accelerated", &self.accelerated)
            .field("popups", &self.popups)
            .field(
                "navigation_filter",
                &self
                    .navigation_filter
                    .as_ref()
                    .map(|_| "Fn(&NavigationRequest) -> bool"),
            )
            .field("js_dialogs", &self.js_dialogs)
            .field(
                "clipboard_access",
                &self.clipboard_access.as_ref().map(|_| "Fn(&str) -> bool"),
            )
            .finish()
    }
}

impl WebviewOptions {
    /// Just a URL, everything else left at its default.
    pub fn url(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }
}

/// What a webview does when the page asks for a new window: `window.open`, a
/// `target="_blank"` link (with `rel="noopener"` or without), or a link the
/// user Cmd-clicks or middle-clicks.
///
/// A native popup window is never created. A windowless browser has nowhere
/// to draw one, and a top-level window the application does not control is
/// not what an embedding application wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopupPolicy {
    /// Do not open it, and emit [`crate::WebviewEvent::PopupRequested`] so the
    /// application can decide what to do instead — hand the URL to the system
    /// browser with `cx.open_url`, say. `window.open` returns `null` to the
    /// page.
    #[default]
    Block,
    /// Load the URL in this webview instead, in place of the current page.
    LoadInPlace,
}

/// What a webview does with the page's JavaScript dialogs — `alert`,
/// `confirm`, `prompt`, and the "leave this page?" dialog `beforeunload` asks
/// for.
///
/// A windowless browser has no dialog of its own to show, so without an answer
/// from somewhere the page cannot go on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DialogPolicy {
    /// Emit [`crate::WebviewEvent::JsDialog`] and wait for
    /// [`crate::Webview::resolve_dialog`].
    ///
    /// The page waits as long as it takes: `alert()` and friends block its
    /// script until they are answered, so an application that never answers
    /// leaves the page stuck. A dialog still waiting when the page navigates
    /// away, or when the webview is dropped, is dismissed.
    #[default]
    Event,
    /// Answer every dialog at once, as if the user pressed Cancel — `alert`
    /// returns, `confirm` returns `false`, `prompt` returns `null` — except the
    /// "leave this page?" dialog, which is allowed to leave so navigation never
    /// gets stuck. No events.
    AutoDismiss,
}

/// Decides whether a navigation may go ahead: `true` lets it through, `false`
/// cancels it and emits [`crate::WebviewEvent::NavigationBlocked`].
///
/// It is asked about every navigation, including the first URL and
/// [`crate::Webview::load_url`], and it is called synchronously from inside
/// the browser engine — so keep it quick, and do not reach back into the
/// webview from it. A filter that panics counts as a refusal.
pub type NavigationFilter = std::sync::Arc<dyn Fn(&NavigationRequest) -> bool + Send + Sync>;

/// Called synchronously with the requesting origin for a Clipboard API
/// permission request. Return true only for trusted origins. Keep it quick
/// and do not call back into the webview. A panic is treated as a refusal.
pub type ClipboardAccessFilter = std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn clipboard_access_allowed(
    filter: Option<&ClipboardAccessFilter>,
    origin: &str,
) -> bool {
    let Some(filter) = filter.filter(|_| !origin.is_empty()) else {
        return false;
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| filter(origin))).unwrap_or_else(|_| {
        log::error!("the clipboard access filter panicked; denying access");
        false
    })
}

/// A navigation about to happen, as a [`NavigationFilter`] sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NavigationRequest {
    /// Where it is going.
    pub url: String,
    /// Whether this is a server redirect of a navigation already under way.
    /// Always `false` with WebView2.
    pub is_redirect: bool,
    /// Whether the user started it — clicked a link, say — rather than the
    /// page or the application. Always `false` with WebView2.
    pub user_gesture: bool,
    /// Whether it is the page itself rather than an `<iframe>` inside it.
    /// Always `true` with WebView2, which only asks about the page.
    pub is_main_frame: bool,
}

/// Asks `filter` about `request`, treating a panic as a refusal: it would
/// otherwise unwind into the browser engine and abort the process.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub(crate) fn navigation_allowed(filter: &NavigationFilter, request: &NavigationRequest) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| filter(request))).unwrap_or_else(
        |_| {
            log::error!("the navigation filter panicked; blocking {}", request.url);
            false
        },
    )
}

/// How [`crate::Webview::print_to_pdf`] lays the page out. The defaults are
/// Chromium's: US Letter, portrait, default margins, no background graphics.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PdfOptions {
    /// Print across the long edge of the paper.
    pub landscape: bool,
    /// Print background colours and images.
    pub print_background: bool,
    /// The paper size in inches, width by height. `None` is US Letter
    /// (8.5 by 11).
    pub paper_size: Option<(f64, f64)>,
    /// Use the size a page sets with CSS `@page`, when it sets one.
    pub prefer_css_page_size: bool,
    /// The margins around each page.
    pub margins: PdfMargins,
    /// Which pages to print, such as `"1-3, 5"`. `None` prints them all.
    pub page_ranges: Option<String>,
}

/// The margins in [`PdfOptions`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum PdfMargins {
    /// Chromium's default, about 0.4 inches.
    #[default]
    Default,
    /// No margins at all.
    None,
    /// These margins, in inches.
    Custom {
        top: f64,
        right: f64,
        bottom: f64,
        left: f64,
    },
}

/// A cookie to put in the browser's store, for [`crate::Runtime::set_cookies`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieSpec {
    /// A URL the cookie belongs to, such as `https://github.com/`; the store
    /// checks the cookie against it.
    pub url: String,
    pub name: String,
    pub value: String,
    /// The domain, with a leading dot for a domain cookie; empty for a
    /// host-only cookie.
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    /// Seconds since the Unix epoch; `None` for a session cookie.
    pub expires: Option<i64>,
    pub same_site: SameSite,
}

/// A cookie's `SameSite` policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SameSite {
    #[default]
    Unspecified,
    None,
    Lax,
    Strict,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(url: &str) -> NavigationRequest {
        NavigationRequest {
            url: url.into(),
            is_redirect: false,
            user_gesture: true,
            is_main_frame: true,
        }
    }

    #[test]
    fn the_filter_decides() {
        let filter: NavigationFilter =
            std::sync::Arc::new(|request| !request.url.contains("blocked"));
        assert!(navigation_allowed(
            &filter,
            &request("https://example.com/")
        ));
        assert!(!navigation_allowed(
            &filter,
            &request("https://blocked.example/")
        ));
    }

    #[test]
    fn a_panicking_filter_blocks_instead_of_unwinding() {
        let filter: NavigationFilter = std::sync::Arc::new(|_| panic!("bug in the filter"));
        assert!(!navigation_allowed(
            &filter,
            &request("https://example.com/")
        ));
    }

    #[test]
    fn options_print_without_their_closures() {
        let options = WebviewOptions {
            navigation_filter: Some(std::sync::Arc::new(|_| true)),
            clipboard_access: Some(std::sync::Arc::new(|_| true)),
            ..WebviewOptions::url("https://example.com/")
        };
        let printed = format!("{options:?}");
        assert!(printed.contains("https://example.com/"));
        assert!(printed.contains("navigation_filter: Some"));
        assert!(printed.contains("clipboard_access: Some"));
    }

    #[test]
    fn clipboard_requires_an_explicit_origin_decision() {
        let options = WebviewOptions::default();
        assert!(!clipboard_access_allowed(
            options.clipboard_access.as_ref(),
            "https://editor.example"
        ));
        let filter: ClipboardAccessFilter =
            std::sync::Arc::new(|origin| origin == "https://editor.example");
        assert!(clipboard_access_allowed(
            Some(&filter),
            "https://editor.example"
        ));
        for origin in [
            "",
            "null",
            "http://editor.example",
            "https://editor.example.evil",
            "https://editor.example:444",
        ] {
            assert!(!clipboard_access_allowed(Some(&filter), origin));
        }
    }

    #[test]
    fn a_panicking_clipboard_filter_refuses_access() {
        let filter: ClipboardAccessFilter = std::sync::Arc::new(|_| panic!("filter bug"));
        assert!(!clipboard_access_allowed(
            Some(&filter),
            "https://editor.example"
        ));
    }
}
