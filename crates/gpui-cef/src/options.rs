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
#[derive(Clone, Debug)]
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
}

impl Default for WebviewOptions {
    fn default() -> Self {
        Self {
            url: "about:blank".into(),
            transparent: false,
            frame_rate: 60,
            accelerated: false,
        }
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
