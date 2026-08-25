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
    gpui::Application::new().run(move |cx| {
        // 2. gpui has created NSApp by now, so initialize CEF here.
        cef.start(cx).unwrap();

        cx.open_window(Default::default(), |window, cx| {
            cx.new(|cx| Webview::new(window, cx, WebviewOptions::url("https://example.com")))
        })
        .unwrap();
    });

    // 3. The message loop is done, so shut CEF down.
    drop(runtime);
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

So the **default is the CPU copy path** (`WebviewOptions::accelerated == false`):
the BGRA that CEF's `OnPaint` returns is copied into a `RenderImage` and drawn with
`img()`. That is roughly 3.9MB per frame at 1200x820.

Making it zero-copy needs a BGRA branch in gpui — a pixel-format check in
`paint_surface` plus a shader that draws BGRA as-is. Only set `accelerated: true`
when running against a gpui that has one.

## Known limitations

- **No IME.** Composition input (`SetComposition`) is not wired up, so typing
  Japanese, Chinese, or Korean into a page will not work.
- **No drag and drop** between the page and the rest of the application.
- **The CPU copy path is the default.** See the section above.
- The cursor the page asks for is forwarded to gpui, but CEF's custom cursors
  (`CT_CUSTOM`) fall back to an arrow.

## Testing

```bash
cargo test --workspace
```

The tests cover the pure parts: the gpui-to-CEF input translation, the shared
state that decides when CEF has to be told about a resize, the popup layer
bookkeeping, the cursor mapping, and the demo's URL handling.

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
    handlers.rs         CEF-side callbacks (RenderHandler and friends)
    input.rs            gpui input events to CEF events
    nsapp.rs            retrofit CefAppProtocol onto NSApp
    pump.rs             drive CEF from a CFRunLoopTimer
    shared.rs           state shared between CEF and gpui
  src/platform/wry/     Windows: keep a WebView2 child window on the layout rect
examples/browser/       demo app (main binary plus the CEF child-process helper)
xtask/                  assembles the .app bundle
```

## Not verified

- **The Windows backend has never been run.** CI compiles and lints it on
  Windows, but nobody has watched it draw anything.
- Real-world keyboard and scrolling behaviour. The translation is unit tested,
  but the round trip through CEF is not.
- More than one webview at a time. The message pump is shared across them by
  design, but only the single-webview case has been exercised.
- How it looks. The development machine had no screen recording permission, so no
  screenshot could capture the window. That the page renders at exactly the
  element's rect was confirmed over CDP instead (994x786 inside a 1280x860
  window, matching the layout to the pixel).
