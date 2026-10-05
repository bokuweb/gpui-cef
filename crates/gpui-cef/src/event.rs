//! What a [`Webview`](crate::Webview) tells the application.

/// What a [`Webview`](crate::Webview) reports to the application, through
/// [`gpui::EventEmitter`]. Subscribe with `cx.subscribe(&webview, ...)`.
///
/// Everything here originates in the page or the browser engine, so treat the
/// strings as untrusted input.
///
/// New kinds of event are added in minor releases, so a `match` needs a
/// wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WebviewEvent {
    /// A message the page posted with the script
    /// [`post_message_script`](crate::post_message_script) returns.
    ///
    /// Untrusted: any script running in the page can post one, so treat it as
    /// input from the page, not from the application.
    Message(String),

    /// The page asked for a new window and [`PopupPolicy::Block`] kept it
    /// from opening.
    ///
    /// [`PopupPolicy::Block`]: crate::PopupPolicy::Block
    #[non_exhaustive]
    PopupRequested {
        /// Where the new window would have gone. Empty for a bare
        /// `window.open()`.
        url: String,
        /// The window name the page gave, as in `window.open(url, "name")` or
        /// `target="name"`. Empty for `_blank` or no name, and always empty
        /// with WebView2.
        target_frame: String,
        /// Whether the user asked for it — clicked a link, say — rather than
        /// the page opening it on its own. Always `false` with WebView2.
        user_gesture: bool,
    },
}
