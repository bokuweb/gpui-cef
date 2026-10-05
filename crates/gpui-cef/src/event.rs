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

    /// [`WebviewOptions::navigation_filter`] turned a navigation down.
    ///
    /// [`WebviewOptions::navigation_filter`]: crate::WebviewOptions::navigation_filter
    #[non_exhaustive]
    NavigationBlocked {
        /// Where it would have gone.
        url: String,
    },

    /// The page opened a JavaScript dialog and is waiting for the answer. Show
    /// something, then pass the answer to
    /// [`Webview::resolve_dialog`](crate::Webview::resolve_dialog).
    ///
    /// Until then the page's script is blocked; see
    /// [`DialogPolicy`](crate::DialogPolicy). macOS (CEF) only.
    #[non_exhaustive]
    JsDialog {
        /// What to answer by.
        id: DialogId,
        /// Which dialog it is.
        kind: JsDialogKind,
        /// What the page wants to say. For the "leave this page?" dialog it is
        /// the browser's own wording, since pages no longer get to choose it.
        message: String,
        /// The text a `prompt()` starts out with. Empty for the others.
        default_prompt: String,
        /// The page that asked, for showing whose dialog this is. Empty for the
        /// "leave this page?" dialog.
        origin_url: String,
    },

    /// The page wants a file chosen — an `<input type="file">` was clicked, or
    /// it called `showOpenFilePicker()` and the like. Show a picker, then pass
    /// the choice to
    /// [`Webview::resolve_file_dialog`](crate::Webview::resolve_file_dialog).
    /// macOS (CEF) only: WebView2 shows its own.
    #[non_exhaustive]
    FileDialog {
        /// What to answer by.
        id: FileDialogId,
        /// What is being chosen.
        mode: FileDialogMode,
        /// A title the page asked for. Usually empty, meaning "Open" or "Save".
        title: String,
        /// Where to start, with a file name in it for [`FileDialogMode::Save`].
        default_path: Option<std::path::PathBuf>,
        /// What the page will accept, as it wrote it in `accept`: lower-case
        /// MIME types (`image/*`) and extensions (`.docx`). Empty means anything.
        accept_filters: Vec<String>,
        /// The same, worked out to file extensions without the dot (`png`,
        /// `docx`), for a picker that filters by extension. Empty when
        /// nothing is known.
        accept_extensions: Vec<String>,
    },
}

/// What a [`WebviewEvent::FileDialog`] asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileDialogMode {
    /// One existing file.
    Open,
    /// Any number of existing files (`<input multiple>`).
    OpenMultiple,
    /// A folder (`<input webkitdirectory>`).
    OpenFolder,
    /// Where to save a new file.
    Save,
}

/// Identifies a [`WebviewEvent::FileDialog`] to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileDialogId(pub(crate) u64);

/// Which JavaScript dialog a [`WebviewEvent::JsDialog`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsDialogKind {
    /// `alert()`: a message and an OK button.
    Alert,
    /// `confirm()`: OK or Cancel. Accepting makes it return `true`.
    Confirm,
    /// `prompt()`: a line of text. Accepting makes it return the text, and
    /// cancelling makes it return `null`.
    Prompt,
    /// The "leave this page?" dialog a `beforeunload` handler asks for.
    /// Accepting leaves the page; cancelling stays.
    BeforeUnload {
        /// Whether the page is being reloaded rather than left.
        is_reload: bool,
    },
}

/// Identifies a [`WebviewEvent::JsDialog`] to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DialogId(pub(crate) u64);
