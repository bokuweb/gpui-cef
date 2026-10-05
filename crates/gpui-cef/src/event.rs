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

    /// The page started a download — a link with `download`, a response the
    /// server marked as an attachment, a `blob:` URL the page built. Say where
    /// it goes with [`Webview::resolve_download`](crate::Webview::resolve_download),
    /// or cancel it; until then it waits. Progress follows as
    /// [`WebviewEvent::DownloadUpdated`].
    ///
    /// With WebView2 this is a notice rather than a question: WebView2 has
    /// already saved it to its default folder, usually the user's Downloads.
    #[non_exhaustive]
    DownloadRequested {
        /// What to answer by. Updates carry the same id.
        id: DownloadId,
        /// Where it comes from. A `blob:` or `data:` URL when the page made it.
        url: String,
        /// The file name the page or the server suggested.
        suggested_name: String,
        /// Its MIME type, when known. Always empty with WebView2.
        mime_type: String,
        /// Its size, when known.
        total_bytes: Option<u64>,
    },

    /// How a download is getting on. Updates to the same download are
    /// coalesced while it is in progress, so not every chunk is reported.
    #[non_exhaustive]
    DownloadUpdated {
        /// The download, as [`WebviewEvent::DownloadRequested`] named it.
        id: DownloadId,
        /// Where it stands.
        state: DownloadState,
        /// How much has arrived.
        received_bytes: u64,
        /// Its size, when known.
        total_bytes: Option<u64>,
        /// Where it is being written, once that is decided.
        path: Option<std::path::PathBuf>,
    },

    /// The process rendering the page went away — it crashed, ran out of
    /// memory, or was killed. The webview keeps showing the last frame and
    /// stops responding; [`Webview::reload`](crate::Webview::reload) starts a
    /// new process and brings the page back. Any JavaScript dialog it had open
    /// is dismissed. macOS (CEF) only.
    #[non_exhaustive]
    RenderProcessTerminated {
        /// How it ended.
        status: TerminationStatus,
        /// The exit code or signal, as Chromium's "Aw, Snap!" page would show
        /// it.
        error_code: i32,
        /// Chromium's description of it, possibly empty.
        error_text: String,
    },

    /// A page or frame failed to load: no network, a name that does not
    /// resolve, a refused connection. A load cancelled by another navigation
    /// or by [`WebviewOptions::navigation_filter`] is not a failure and is not
    /// reported. macOS (CEF) only.
    ///
    /// [`WebviewOptions::navigation_filter`]: crate::WebviewOptions::navigation_filter
    #[non_exhaustive]
    LoadFailed {
        /// What failed to load.
        url: String,
        /// Chromium's network error code, negative: `-105` is
        /// `ERR_NAME_NOT_RESOLVED`, for instance.
        error_code: i32,
        /// The same as text, such as `ERR_NAME_NOT_RESOLVED`.
        error_text: String,
        /// Whether it was the page rather than an `<iframe>` inside it.
        is_main_frame: bool,
    },
}

/// How the process rendering a page ended, in
/// [`WebviewEvent::RenderProcessTerminated`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TerminationStatus {
    /// It exited with an error.
    Abnormal,
    /// It was killed, by the OS or a task manager.
    Killed,
    /// It crashed.
    Crashed,
    /// It ran out of memory.
    OutOfMemory,
    /// It could not be started.
    LaunchFailed,
    /// Its code failed an integrity check.
    IntegrityFailure,
}

/// Where a download stands, in a [`WebviewEvent::DownloadUpdated`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadState {
    /// Still arriving.
    InProgress,
    /// All of it is on disk.
    Completed,
    /// Cancelled, by the application or the browser. Nothing more will come.
    Canceled,
    /// Stopped by an error, such as a dropped connection.
    Failed,
}

/// Identifies a download, from [`WebviewEvent::DownloadRequested`] on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DownloadId(pub(crate) u64);

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
