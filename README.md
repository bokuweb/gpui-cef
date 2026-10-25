# gpui-cef

[![CI](https://github.com/bokuweb/gpui-cef/actions/workflows/ci.yml/badge.svg)](https://github.com/bokuweb/gpui-cef/actions/workflows/ci.yml)

Embed a real browser engine into [gpui](https://github.com/zed-industries/zed/tree/main/crates/gpui).

| OS | Engine | How it is composited |
|---|---|---|
| macOS | **Chromium (CEF 151)** | Rendered off-screen and drawn as an ordinary gpui element, so it is clipped and layered like anything else |
| Windows | **WebView2 (wry)** | A native child window kept aligned with the laid-out rect; always in front of gpui's own drawing |

## Why Chromium on macOS

Using wry — as in [this post about gpui-webview](https://voluntas.ghost.io/gpui-webview/) —
attaches a `WKWebView` as a **child view of the gpui window**. That is easy, but it means:

- nothing can be drawn on top of the webview
- rounded corners and masks do not clip it
- it is not part of gpui's scene, so it does not stay in sync with animation or scrolling

CEF supports off-screen rendering, which turns the page into just another gpui
element. The demo layers a gpui badge over the webview to show this.

WebView2 exposes no public off-screen API, so the Windows backend uses the
straightforward child-window approach — meaning **those limitations remain on
Windows**. The API is the same; the compositing behaviour is not.

## Usage

```rust
use gpui_cef::{RuntimeOptions, Webview, WebviewOptions};

fn main() {
    // 1. At the top of main: load CEF into the process.
    let runtime = gpui_cef::init(RuntimeOptions::default()).unwrap();

    // Hand the closure a *clone*. Moving the value itself in would run
    // cef_shutdown() the moment the closure returns, taking the process with it.
    let cef = runtime.clone();
    gpui_platform::application().run(move |cx| {
        // 2. gpui has created NSApp by now, so initialize CEF here.
        cef.start(cx).unwrap();

        cx.open_window(Default::default(), |window, cx| {
            cx.new(|cx| Webview::new(window, cx, WebviewOptions::url("https://example.com")))
        })
        .unwrap();
    });

    // 3. The message loop is done, so shut CEF down.
    runtime.shutdown();
}
```

`Webview` is an ordinary gpui entity, so `div().child(webview.clone())` mixes it
with anything else.

### API

| | macOS (CEF) | Windows (WebView2) |
|---|---|---|
| `load_url` / `reload` / `go_back` / `go_forward` / `stop` | yes | yes |
| `eval` (run JavaScript) | yes | yes |
| `url` | yes | yes |
| `title` | yes | no — always empty |
| `is_loading` | yes | approximated from page load events |
| `can_go_back` / `can_go_forward` | yes | no — always `true` |
| `WebviewEvent::Message` (page to app, via `post_message_script`) | yes | yes |
| `Runtime::set_cookies` | yes | no — `Error::Unsupported` |
| Popups: `WebviewOptions::popups`, `WebviewEvent::PopupRequested` | yes | yes — `target_frame` is always empty and `user_gesture` always `false` |
| `WebviewOptions::navigation_filter`, `WebviewEvent::NavigationBlocked` | yes, for the page and its frames | the page only; `is_redirect` and `user_gesture` are always `false` |
| JavaScript dialogs: `WebviewEvent::JsDialog`, `resolve_dialog`, `WebviewOptions::js_dialogs` | yes | no — WebView2 shows its own |
| File choosers: `WebviewEvent::FileDialog`, `resolve_file_dialog` | yes, except folders (see below) | no — WebView2 shows its own |
| Downloads: `WebviewEvent::DownloadRequested`, `resolve_download` | yes; the download waits for the answer | a notice only: WebView2 has already saved it to its default folder, and `resolve_download` does nothing |
| `WebviewEvent::DownloadUpdated` | progress, completion, cancellation, failure | completion and failure only |
| `WebviewEvent::RenderProcessTerminated` | yes | no |
| `WebviewEvent::LoadFailed` | yes | no |
| `print_to_pdf`, `WebviewEvent::PdfPrinted` | yes | no — `Error::Unsupported` |

A page talks back to the application through its console: the script
`post_message_script(expr)` returns logs `expr` with a prefix, and the webview
emits it as `WebviewEvent::Message`. Anything in the page can do this, so treat
a message as input from the page.

`gpui` comes from Zed's repository, as it does for gpui-component: the
application's lockfile picks the revision, so the app and this crate link one
`gpui`. This repository's own lockfile pins the revision CI builds against.

### When the page needs the application

A windowless browser has no window of its own to put a dialog, a file
picker, a popup or a "Save As" panel in, so whatever the page asks for that
needs one comes to the application as a `WebviewEvent`, and the application
answers through the webview:

```rust
use std::sync::Arc;
use gpui::PromptLevel;
use gpui_cef::{JsDialogKind, Webview, WebviewEvent, WebviewOptions};

let webview = cx.new(|cx| {
    Webview::new(window, cx, WebviewOptions {
        // Asked about every navigation before it starts.
        navigation_filter: Some(Arc::new(|request| !request.url.starts_with("http://"))),
        ..WebviewOptions::url("https://example.com")
    })
});

cx.subscribe_in(&webview, window, |_, webview, event, window, cx| match event {
    // Popups never open a window; send them to the system browser instead.
    WebviewEvent::PopupRequested { url, .. } => cx.open_url(url),
    WebviewEvent::JsDialog { id, kind: JsDialogKind::Confirm, message, .. } => {
        let (id, webview) = (*id, webview.downgrade());
        let answer = window.prompt(PromptLevel::Info, message, None, &["OK", "Cancel"], cx);
        cx.spawn(async move |_, cx| {
            let accept = answer.await == Ok(0);
            webview.update(cx, |webview, _| webview.resolve_dialog(id, accept, None)).ok();
        })
        .detach();
    }
    WebviewEvent::DownloadRequested { id, suggested_name, .. } => {
        // ... cx.prompt_for_new_path(...), then
        // webview.read(cx).resolve_download(id, Some(path)) — or None to cancel.
    }
    WebviewEvent::RenderProcessTerminated { .. } => webview.read(cx).reload(),
    _ => {}
})
.detach();
```

- **Answer each request once**, by the id its event carried: `resolve_dialog`,
  `resolve_file_dialog`, `resolve_download`. They return `false` for a request
  that is no longer waiting.
- **The page waits for the answer.** `alert()` and friends block the page's
  script until they are answered, so an application that ignores
  `JsDialog` leaves the page stuck; set `js_dialogs: DialogPolicy::AutoDismiss`
  to have every dialog dismissed on the spot instead. A dialog still open when
  the page navigates away is dismissed.
- **Nothing is left hanging when the webview goes.** Dropping it dismisses open
  dialogs, cancels file choosers, and cancels downloads that were never told
  where to go; anything the page asks for after that is turned down.
- **Popups never get a window.** By default they are blocked and reported;
  `popups: PopupPolicy::LoadInPlace` loads them in the same webview.
- **The navigation filter runs inside the browser engine**, synchronously, so
  keep it quick and do not reach back into the webview from it. A panicking
  filter counts as "no".
- **A crashed renderer leaves the last frame on screen.** `reload()` starts a
  new one and brings the page back.
- **Print through `print_to_pdf`.** The page's own `window.print()` puts the
  system print panel up as a window of its own, detached from the gpui window,
  and holds the page until it closes.

CEF calls these handlers on the main thread — with `external_message_pump` it
is CEF's UI thread — from inside the message pump. They only queue events, and
the pump emits them through gpui within a frame; the `resolve_*` calls answer
CEF right away, on the same thread.

### The demo (`examples/browser`)

`just run` launches it. The toolbar, address bar, and side panel are all drawn by
gpui; whether the history buttons are enabled and whether a load is in flight come
straight from CEF's `LoadHandler`.

- Type in the address bar and press Enter to navigate (input without a `.` is
  treated as a search term)
- Back, forward, and reload. The buttons grey out when there is no history
- A side panel button runs `eval` to inject JavaScript into the page (colour inversion)
- Toggling the side panel changes the webview's width, and CEF's render size follows

The webview is **clipped to rounded corners** and a **gpui element is drawn over
it** in the bottom right. A child-view backend can do neither, which is the reason
this crate exists.

The side panel's **Feature test page** has a button for everything the webview
hands to the application, and the demo answers each natively: gpui prompts for
`alert`, `confirm` and "leave this page?", a gpui overlay with a text field for
`prompt`, open and save panels for file choosers and downloads, the system
browser for popups, a Reload banner after a renderer crash, and a save panel for
printing to PDF. A navigation filter turns `blocked.example` away. The side panel
lists the latest events. `GPUI_CEF_START_URL=features` starts on that page.

## Upgrading from 0.1

- `WebviewEvent` is `#[non_exhaustive]`, and so are its struct-like variants:
  give a `match` a `_ => {}` arm, and match variants with `..`.
- `WebviewOptions` has new fields; build it with `WebviewOptions::url(..)` or
  `..Default::default()` as before.
- **Behaviour changes on macOS.** Popups no longer open a native window; they
  are blocked and reported as `PopupRequested`. JavaScript dialogs now wait for
  `resolve_dialog`: an application that does not handle `JsDialog` should set
  `js_dialogs: DialogPolicy::AutoDismiss`, or `alert()` will block the page.
  Downloads, which used to be cancelled, now wait for `resolve_download`.
- Page-to-app messages (`post_message_script`, `WebviewEvent::Message`) are
  unchanged. A page that worked around the missing pieces through them can drop
  the workarounds: an overridden `window.open` for `PopupRequested`, a
  `confirm()` bridged through messages for `JsDialog`, a heartbeat watchdog for
  `RenderProcessTerminated`, intercepted blob downloads for `DownloadRequested`,
  a hand-rolled file input for `FileDialog`.

## How frames get across, and what zero-copy still needs

With `shared_texture_enabled`, CEF hands over its output as a **shared IOSurface**.
gpui has a `surface()` element that composites a `CVPixelBuffer` directly as a
Metal texture, so bridging the two with `CVPixelBufferCreateWithIOSurface` should
make this zero-copy. That was the original plan, and **everything up to gpui does
work**: CEF to IOSurface to `CVPixelBuffer` to gpui is verified.

It stops at the last step on stock gpui. `surface()` hard-codes NV12
(`kCVPixelFormatType_420YpCbCr8BiPlanarFullRange`) and draws the Y and CbCr planes
through a YUV-to-RGB shader — it was written for Zed's screen sharing. CEF produces
BGRA, so passing it straight through trips an `assert_eq!` in `metal_renderer.rs`.

Measured cost of that copy, at 994x786 in a debug build: **1.2% CPU across all
processes when the page is idle, ~38% while the page repaints continuously at
60fps**. An application UI is idle almost all the time, so the copy rarely shows
up; a page running constant animation is where it does.

So the **default is the CPU copy path** (`WebviewOptions::accelerated == false`):
the BGRA that CEF's `OnPaint` returns is copied into a `RenderImage` and drawn with
`img()`. That is roughly 3.9MB per frame at 1200x820.

Making it zero-copy needs a BGRA branch in gpui — a pixel-format check in
`paint_surface` plus a shader that draws BGRA as-is. Only set `accelerated: true`
when running against a gpui that has one.

## Known limitations

- **No drag and drop** between the page and the rest of the application.
- **Folder uploads do not complete.** `<input webkitdirectory>` reaches the
  application as a `FileDialog` in `OpenFolder` mode, but after the answer
  Chromium asks the user to confirm the upload in a dialog a windowless browser
  cannot show, and the page never receives the files.
- **One download at a time without a click.** Chromium holds a page's second
  download until the user has interacted with the page again, and asks for
  permission in the meantime; Alloy-style CEF has no prompt for that, so a page
  that starts several downloads in a row without user input gets the first.
  Permission prompts in general (camera, microphone, notifications) are not
  handled yet, which with Alloy means they are not granted.
- **`window.print()`** opens the system print panel detached from the window;
  see above.
- **The CPU copy path is the default.** See the section above.
- The cursor the page asks for is forwarded to gpui, but CEF's custom cursors
  (`CT_CUSTOM`) fall back to an arrow.
- Input methods are wired up (see below) but the live round trip has not been
  confirmed by a human at a keyboard.

## Text input and input methods

Off-screen rendering leaves CEF with no window for the OS to attach an input
context to, so Chromium never sees the input method on its own. Sending key
events alone gets you ASCII and nothing else — Japanese, Chinese, Korean and
dead-key accents all arrive as *composition*, which the OS negotiates with the
focused view instead of delivering as keystrokes.

`platform/cef/ime.rs` implements gpui's `EntityInputHandler` against CEF's
`ImeSetComposition` / `ImeCommitText` / `ImeCancelComposition`, and uses the
rects from `OnImeCompositionRangeChanged` to place the candidate window. That
trait is written for an editor that owns its text, which this does not — but the
OS only ever asks about the text it is currently composing, and a mirror of just
that much is enough.

Text is sent only after the platform input handler accepts it. Ordinary BMP text
uses CEF `CHAR` events; composition commits and text containing astral Unicode
characters use `ImeCommitText`. A `CHAR` holds only one UTF-16 unit, so sending a
surrogate pair as two such events exposes ill-formed `beforeinput` / `input` data
to editors even when a plain input's eventual value looks correct. Astral text
is committed atomically, including when there is no active composition. The
native input probe checks each DOM input event as well as the final value.

## Testing

```bash
cargo test --workspace
```

The tests cover the pure parts: the gpui-to-CEF input translation, the shared
state that decides when CEF has to be told about a resize, the popup layer
bookkeeping, the cursor mapping, the bookkeeping of requests waiting on the
application (ids, answering once, cancelling what is left), the event queue,
the translation of CEF's dialog, download and termination types, and the
demo's URL handling.

```bash
just e2e
```

checks the rest end to end on macOS. It bundles the demo, starts it with
`GPUI_CEF_DEMO_AUTO=1` — which answers every dialog, file chooser and download
on the spot rather than showing anything — on the feature test page, and clicks
through the page over the remote debugging port with a small CDP client
(`examples/browser/e2e.mjs`, Node 22, no dependencies). Each check looks at both
ends: the event the demo logged, and what the page or the disk saw after the
answer. It opens a real window, so it is not part of CI.

```bash
just input-e2e
```

checks the actual AppKit `NSTextInputClient` → GPUI → CEF path on macOS,
including accented characters, surrogate pairs, composition updates, commit,
finish and cancellation. The Node wrapper copies the bundle into a temporary
directory, changes its identifier, marks it background-only, signs it and
removes it afterwards. The test uses a disposable CEF profile, no debugging
port, a hidden window and logical browser focus without activating the app.
It invokes the native text-client callbacks and asserts the renderer's DOM
values and UTF-16 marked ranges. It also sends synthetic typing, Backspace, Left and
Delete NSEvents directly to its own view and checks typing, caret movement and
deletion of complete surrogate pairs.
Five additional key pairs go through the owned `NSWindow.sendEvent:` and its
first responder, with exact DOM `keydown`/`keyup` pairs checked as well as the
resulting text. A self-tested observer records `NSResponder.noResponderFor:`
in this disposable process and fails on any unexpected invocation. It suppresses
that method's default alert only in the test process; it does not change the
library or prove the absence of all warning sounds. The hidden input test runs
in macOS CI. It does not post system keyboard events or switch the system input
source. Physical keyboard input, IME candidate UI, input latency and audio remain
unverified. A macOS WindowServer/Metal session is still required.

Anything that needs a live browser is exercised by running the demo instead.
CEF's remote debugging port is open, so `http://127.0.0.1:9229/json/list` plus
CDP can drive and inspect the page without looking at the screen — useful both in
a headless environment and for checking things that are hard to eyeball. That is
how the layout rect (994x786 inside a 1280x860 window) and the `<select>` popup
were verified.

## Building

### Prerequisites

| | Why | How |
|---|---|---|
| cmake / ninja | building `libcef_dll_wrapper` | `brew install cmake ninja` |
| CEF binaries (~500MB) | downloaded into `$CEF_PATH` by `cef-dll-sys` on the first build | automatic |

**Xcode.app is not required.** gpui compiles its shaders with `xcrun metal` at
build time by default, and that tool does not ship with the Command Line Tools.
This repository enables gpui's `runtime_shaders` feature, which hands the shader
source to Metal at runtime instead.

`just doctor` checks all of this.

### Commands

```bash
just doctor   # check the prerequisites
just bundle   # build the .app bundle
just run      # bundle and launch
```

On macOS you **must go through the bundle**. CEF locates
`Contents/Frameworks/Chromium Embedded Framework.framework` and
`Contents/Frameworks/<name> Helper.app` through the bundle layout, so a bare
executable from `cargo run` cannot start.

### Environment-specific gotcha

If building `libcef_dll_wrapper` fails with `fatal error: 'atomic' file not found`,
a stale libc++ in `/Library/Developer/CommandLineTools/usr/include/c++/v1` is
shadowing the complete one in the SDK. The `justfile` works around it through
`CXXFLAGS`, but the real fix is reinstalling the Command Line Tools.

## Implementation notes (the mines that were stepped on)

gpui and CEF both assume they own the foundations of the application, so making
them coexist takes some care. Every item below was found by crashing into it.

### Fighting over NSApplication

There is one `NSApp` per process, and it is instantiated as **whichever class
calls `sharedApplication` first**. Initializing CEF first produces a plain
`NSApplication`, and gpui then crashes because the instance it fetches through
`[GPUIApplication sharedApplication]` has none of its own ivars.

Fix: split initialization in two and let gpui create `NSApp` before CEF is
initialized (`init` then `Runtime::start`).

### CefAppProtocol

CEF and Chromium both CHECK
`[NSApp conformsToProtocol:@protocol(CefAppProtocol)]` and abort with a SIGTRAP if
it fails. gpui's class naturally knows nothing about it — and the protocol only
exists in headers, so it is not even registered with the Objective-C runtime.

Fix: register a protocol of the same name with `objc_allocateProtocol`, add
`isHandlingSendEvent` / `setHandlingSendEvent:` implementations, and swizzle
`sendEvent:` to raise the flag (`platform/cef/nsapp.rs`).

### The message loop

Calling `do_message_loop_work()` while gpui's `App` is borrowed lets gpui's paint
callbacks re-enter from the run loop Chromium spins, which panics with
`RefCell already borrowed`.

On top of that, gpui's foreground executor barely runs tasks while the app is idle
— measured at one tick every one to five seconds. Putting the pump there deadlocks
the whole thing: the pump does not run, so CEF produces no frames, so gpui does not
repaint, so the executor does not run.

Fix: bypass gpui and install a `CFRunLoopTimer` on the main run loop at 60Hz
(`platform/cef/pump.rs`). The callback comes straight from the run loop, so nobody
is borrowing gpui's `App`.

### The child process sandbox

`cef-dll-sys` is built with `USE_SANDBOX=ON` by default. Without a
`cef_sandbox_initialize` call in the helper, the GPU and network child processes
die instantly with `exit_code=5`, and then nothing loads and nothing paints. The
only symptom is a "GPU process exited unexpectedly" line, which makes it easy to
miss.

### Runtime lifetime

Moving `Runtime` into the `Application::run` closure runs `cef_shutdown()` **the
moment the closure returns** — that is, right after startup — killing Chromium
mid-launch with a SIGTRAP.

Fix: make `Runtime` an `Rc`-backed shared handle and pass the closure a clone.

## Layout

```
crates/gpui-cef/
  src/platform/cef/     macOS: draw CEF's off-screen output as a gpui element
    mod.rs              init / Runtime / Webview / Element
    handlers.rs         CEF-side callbacks: rendering, display, loading, popups,
                        navigation, dialogs, file choosers, downloads, crashes
    pending.rs          requests waiting on the application's answer
    input.rs            gpui input events to CEF events
    nsapp.rs            retrofit CefAppProtocol onto NSApp
    pump.rs             drive CEF from a CFRunLoopTimer
    shared.rs           state shared between CEF and gpui
  src/platform/wry/     Windows: keep a WebView2 child window on the layout rect
examples/browser/       demo app (main binary plus the CEF child-process helper),
                        its feature test page, and the end-to-end check
xtask/                  assembles the .app bundle
```

## Not verified

- **The Windows backend has never been run.** CI compiles and lints it on
  Windows, but nobody has watched it draw anything — popups, the navigation
  filter and downloads through wry included.
- **The demo's native answers.** `just e2e` answers automatically; that the
  gpui prompt, the open and save panels and the `prompt()` overlay come up has
  been seen in the window list, but clicking them has been left to a human.
- **Typing.** The input-method wiring is unit tested for its offset arithmetic
  only. Synthesizing real key events needs macOS Accessibility permission, which
  the development machine does not have, so the round trip from keyboard to page
  has to be confirmed by hand.
- Scrolling direction and step size through CEF.
- More than one webview at a time. The message pump is shared across them by
  design, but only the single-webview case has been exercised.
- How it looks. The development machine had no screen recording permission, so no
  screenshot could capture the window. That the page renders at exactly the
  element's rect was confirmed over CDP instead (994x786 inside a 1280x860
  window, matching the layout to the pixel).

## License

Licensed under either the [Apache License, Version 2.0](LICENSE-APACHE) or the
[MIT license](LICENSE-MIT), at your option, as declared by the workspace's Cargo
metadata. Dependencies, including the bundled browser engine, retain their own
licenses.
