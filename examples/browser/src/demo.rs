//! The parts of the demo that are not drawing: the feature test page, the
//! navigation filter, and how events read in the log.

use std::path::PathBuf;

use gpui_cef::{NavigationRequest, WebviewEvent};

/// The feature test page, with a button for each thing the webview hands to
/// the application.
const FEATURES_HTML: &str = include_str!("features.html");

/// How the demo answers what the page asks.
pub(crate) struct Demo {
    /// `GPUI_CEF_DEMO_AUTO=1`: answer every dialog, file chooser and download
    /// on the spot, without showing anything, so a script can drive the page
    /// over the remote debugging port and check the whole round trip. Files
    /// go to [`Demo::dir`].
    pub(crate) auto: bool,
    /// Where the feature test page and the automatic answers live.
    pub(crate) dir: PathBuf,
}

impl Demo {
    pub(crate) fn from_env() -> Self {
        Self {
            auto: std::env::var_os("GPUI_CEF_DEMO_AUTO").is_some(),
            dir: std::env::temp_dir().join("gpui-cef-demo"),
        }
    }

    /// Writes the feature test page out and returns its `file://` URL. The
    /// page reaches the application through the console, so it is given the
    /// prefix the webview listens for.
    pub(crate) fn features_url(&self) -> std::io::Result<String> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join("features.html");
        let html = FEATURES_HTML.replace("__GPUI_CEF_MESSAGE_PREFIX__", gpui_cef::MESSAGE_PREFIX);
        std::fs::write(&path, html)?;
        Ok(format!("file://{}", path.display()))
    }

    /// A text file to hand to the page's file chooser when answering
    /// automatically.
    pub(crate) fn sample_file(&self) -> PathBuf {
        let path = self.dir.join("picked.txt");
        if let Err(err) = std::fs::write(&path, "Picked by the gpui-cef demo\n") {
            log::error!("could not write {}: {err}", path.display());
        }
        path
    }

    /// Where an automatic answer saves a download or a PDF.
    pub(crate) fn output(&self, name: &str) -> PathBuf {
        let dir = self.dir.join("out");
        if let Err(err) = std::fs::create_dir_all(&dir) {
            log::error!("could not create {}: {err}", dir.display());
        }
        dir.join(name)
    }
}

/// Whether the feature test page wants this one turned down, to show that
/// path in automatic mode: its declined dialog and download say "decline".
pub(crate) fn asks_to_decline(text: &str) -> bool {
    text.to_lowercase().contains("decline")
}

/// The demo's navigation filter: anything except `blocked.example`, which
/// the feature test page links to.
pub(crate) fn allow_navigation(request: &NavigationRequest) -> bool {
    allow_url(&request.url)
}

fn allow_url(url: &str) -> bool {
    host(url) != Some("blocked.example")
}

fn host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    Some(host.split(':').next().unwrap_or(host))
}

/// One line for the side panel's event log.
pub(crate) fn describe(event: &WebviewEvent) -> String {
    match event {
        WebviewEvent::Message(message) => format!("Message: {}", shorten(message)),
        WebviewEvent::PopupRequested {
            url, user_gesture, ..
        } => {
            let gesture = if *user_gesture { "clicked" } else { "scripted" };
            format!("Popup blocked ({gesture}): {}", shorten(url))
        }
        WebviewEvent::NavigationBlocked { url, .. } => {
            format!("Navigation blocked: {}", shorten(url))
        }
        WebviewEvent::JsDialog { kind, message, .. } => {
            format!("Dialog {kind:?}: {}", shorten(message))
        }
        WebviewEvent::FileDialog {
            mode,
            accept_extensions,
            ..
        } => {
            if accept_extensions.is_empty() {
                format!("File chooser {mode:?}")
            } else {
                format!("File chooser {mode:?} for {}", accept_extensions.join(", "))
            }
        }
        WebviewEvent::DownloadRequested { suggested_name, .. } => {
            format!("Download requested: {suggested_name}")
        }
        WebviewEvent::DownloadUpdated {
            state,
            received_bytes,
            ..
        } => format!("Download {state:?}: {received_bytes} bytes"),
        WebviewEvent::RenderProcessTerminated {
            status, error_code, ..
        } => format!("Renderer gone: {status:?} ({error_code})"),
        WebviewEvent::LoadFailed {
            url, error_text, ..
        } => format!("Load failed: {} ({error_text})", shorten(url)),
        WebviewEvent::PdfPrinted { path, ok, .. } => {
            let result = if *ok { "written" } else { "failed" };
            format!("PDF {result}: {}", path.display())
        }
        other => format!("{other:?}"),
    }
}

/// Keeps a log line to something the side panel can show.
fn shorten(text: &str) -> String {
    const MAX: usize = 60;
    match text.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_blocked_example_is_turned_down() {
        assert!(!allow_url("https://blocked.example/"));
        assert!(!allow_url("http://user@blocked.example:8080/path?q"));
        assert!(allow_url("https://example.com/"));
        assert!(allow_url("https://not-blocked.example/"));
        assert!(allow_url("file:///tmp/gpui-cef-demo/features.html"));
        assert!(allow_url("about:blank"));
    }

    #[test]
    fn long_lines_are_cut() {
        let url = format!("https://example.com/{}", "a".repeat(100));
        let line = describe(&WebviewEvent::Message(url));
        assert!(line.ends_with('…'));
        assert!(line.chars().count() < 90);
        assert_eq!(shorten("short"), "short");
    }
}
