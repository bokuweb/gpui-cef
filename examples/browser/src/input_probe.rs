//! Real NSTextInputClient callbacks, without sending events to the user's Mac.
//! The wrapper supplies a disposable background-only bundle and report directory.
use gpui::{prelude::*, px, size, Bounds, Focusable, WindowBounds, WindowOptions};
use gpui_cef::{RuntimeOptions, Webview, WebviewEvent, WebviewOptions};
use objc2::{
    msg_send,
    runtime::{AnyClass, AnyObject, Bool, Sel},
    sel,
};
use objc2_foundation::{NSBundle, NSPoint, NSString};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

enum Input {
    Insert(&'static str),
    Mark(&'static str),
    Finish,
    Key(&'static str, u16),
}

// GPUI registers its legacy NSRange spelling, while objc2-foundation uses
// _NSRange. Match the actual client's encoding as well as its C layout.
#[repr(C)]
#[derive(Clone, Copy)]
struct ClientRange {
    location: usize,
    length: usize,
}

unsafe impl objc2::Encode for ClientRange {
    const ENCODING: objc2::Encoding = objc2::Encoding::Struct(
        "NSRange",
        &[
            <usize as objc2::Encode>::ENCODING,
            <usize as objc2::Encode>::ENCODING,
        ],
    );
}

impl ClientRange {
    fn new(location: usize, length: usize) -> Self {
        Self { location, length }
    }
}

pub fn run(report: PathBuf) {
    // Fail before creating NSApp unless the test wrapper isolated this bundle.
    let bundle = NSBundle::mainBundle();
    assert_eq!(
        bundle.bundleIdentifier().unwrap().to_string(),
        "dev.bokuweb.gpui-cef.input-probe"
    );
    let background = bundle
        .objectForInfoDictionaryKey(&NSString::from_str("LSBackgroundOnly"))
        .unwrap();
    let allowed: Bool = unsafe { msg_send![&*background, boolValue] };
    assert!(allowed.as_bool());
    prohibit_activation();

    let page = report.with_file_name("input.html");
    std::fs::write(
        &page,
        r#"<!doctype html><meta charset=utf-8><input id=e autofocus><script>
        window.keyEvents=[]; for(const t of ['keydown','keyup','beforeinput','input'])
        document.addEventListener(t,e=>keyEvents.push([t,e.key,e.code,e.inputType]));
        </script>"#,
    )
    .unwrap();
    let runtime = gpui_cef::init(RuntimeOptions {
        cache_path: Some(report.with_file_name("cef-profile")),
        ..Default::default()
    })
    .unwrap();
    let cef = runtime.clone();
    gpui_platform::application().run(move |cx| {
        let app: *mut AnyObject =
            unsafe { msg_send![AnyClass::get(c"NSApplication").unwrap(), sharedApplication] };
        let _: Bool = unsafe { msg_send![app, setActivationPolicy: 2isize] };
        cef.start(cx).unwrap();
        let handle = cx
            .open_window(
                WindowOptions {
                    show: false,
                    focus: false,
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(640.), px(480.)),
                    ))),
                    ..Default::default()
                },
                |window, cx| {
                    cx.new(|cx| {
                        Webview::new(
                            window,
                            cx,
                            WebviewOptions::url(format!("file://{}", page.display())),
                        )
                    })
                },
            )
            .unwrap();
        let webview = handle.entity(cx).unwrap();
        let view = cx
            .update_window(handle.into(), |_, window, cx| {
                let focus = webview.read(cx).focus_handle(cx);
                // Logical GPUI/CEF focus only; never make the NSWindow key or visible.
                window.focus(&focus, cx);
                let _ = window.draw(cx);
                let RawWindowHandle::AppKit(raw) =
                    HasWindowHandle::window_handle(window).unwrap().as_raw()
                else {
                    panic!("expected AppKit")
                };
                raw.ns_view.as_ptr() as *mut AnyObject
            })
            .unwrap();
        let replies = Rc::new(RefCell::new(Vec::<String>::new()));
        let received = replies.clone();
        cx.subscribe(&webview, move |_, event: &WebviewEvent, _| {
            if let WebviewEvent::Message(message) = event {
                received.borrow_mut().push(message.clone());
            }
        })
        .detach();
        cx.spawn(async move |cx| {
            let steps = [
                (Input::Insert("Aé😀"), "Aé😀", 0),
                (Input::Mark("に"), "Aé😀に", 1),
                (Input::Mark("日本😀"), "Aé😀日本😀", 4),
                (Input::Insert("日本😀"), "Aé😀日本😀", 0),
                (Input::Insert("!"), "Aé😀日本😀!", 0),
                (Input::Mark("語"), "Aé😀日本😀!語", 1),
                (Input::Finish, "Aé😀日本😀!語", 0),
                (Input::Mark("消"), "Aé😀日本😀!語消", 1),
                (Input::Mark(""), "Aé😀日本😀!語", 0),
                (Input::Key("\u{7f}", 51), "Aé😀日本😀!", 0),
                (Input::Key("\u{f702}", 123), "Aé😀日本😀!", 0),
                (Input::Insert("?"), "Aé😀日本😀?!", 0),
                (Input::Key("\u{f728}", 117), "Aé😀日本😀?", 0),
                (Input::Key("\u{7f}", 51), "Aé😀日本😀", 0),
                (Input::Key("\u{7f}", 51), "Aé😀日本", 0),
                (Input::Insert("😀"), "Aé😀日本😀", 0),
                (Input::Key("a", 0), "Aé😀日本😀a", 0),
                (Input::Key("\u{7f}", 51), "Aé😀日本😀", 0),
            ];
            let mut values = Vec::new();
            // Wait for the real renderer/document before invoking the input client.
            wait_for_value(&webview, &replies, "", 0, cx).await;
            for (index, (input, expected, marked_length)) in steps.into_iter().enumerate() {
                assert_hidden(app, view);
                // This is outside App::update: AppKit's callback re-enters GPUI.
                invoke(view, input, marked_length);
                wait_for_value(&webview, &replies, expected, index + 1, cx).await;
                values.push(expected);
            }
            assert_hidden(app, view);
            std::fs::write(
                &report,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "values": values, "visible": false, "activationPolicy": 2,
                    "syntheticNativeKeyEventPairs": 7,
                    "realKeyboardImeOrAudioVerified": false,
                }))
                .unwrap(),
            )
            .unwrap();
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
    runtime.shutdown();
}

fn prohibit_activation() {
    // GPUI requests Regular during launch, before invoking our startup closure.
    // Override that request in this disposable process so even its Dock tile
    // cannot appear. All input-client behavior stays on the actual GPUI view.
    unsafe extern "C-unwind" fn policy(this: &AnyObject, _: Sel, _: isize) -> Bool {
        let superclass = AnyClass::get(c"NSApplication").unwrap();
        unsafe { msg_send![super(this, superclass), setActivationPolicy: 2isize] }
    }
    unsafe {
        let class = AnyClass::get(c"GPUIApplication").unwrap();
        let method = AnyClass::get(c"NSApplication")
            .unwrap()
            .instance_method(sel!(setActivationPolicy:))
            .unwrap();
        assert!(objc2::ffi::class_addMethod(
            class as *const _ as *mut _,
            sel!(setActivationPolicy:),
            std::mem::transmute::<
                unsafe extern "C-unwind" fn(&AnyObject, Sel, isize) -> Bool,
                unsafe extern "C-unwind" fn(),
            >(policy),
            objc2::ffi::method_getTypeEncoding(method),
        )
        .as_bool());
    }
}

fn invoke(view: *mut AnyObject, input: Input, marked_length: usize) {
    let invalid = ClientRange::new(isize::MAX as usize, 0);
    unsafe {
        match input {
            Input::Insert(text) => {
                let text = NSString::from_str(text);
                let _: () = msg_send![view, insertText: &*text, replacementRange: invalid];
            }
            Input::Mark(text) => {
                let selection = ClientRange::new(text.encode_utf16().count(), 0);
                let text = NSString::from_str(text);
                let _: () = msg_send![view, setMarkedText: &*text, selectedRange: selection, replacementRange: invalid];
            }
            Input::Finish => {
                let _: () = msg_send![view, unmarkText];
            }
            Input::Key(characters, key_code) => {
                send_native_key(view, characters, key_code);
            }
        }
        let marked: ClientRange = msg_send![view, markedRange];
        assert_eq!(marked.length, marked_length, "UTF-16 marked range");
        let has_marked: Bool = msg_send![view, hasMarkedText];
        assert_eq!(has_marked.as_bool(), marked_length > 0);
    }
}

unsafe fn send_native_key(view: *mut AnyObject, characters: &str, key_code: u16) {
    // Construct NSEvents for this test window only. Never post them to NSApp,
    // the global event queue, or the user's active application.
    let characters = NSString::from_str(characters);
    unsafe {
        let window: *mut AnyObject = msg_send![view, window];
        let number: isize = msg_send![window, windowNumber];
        for event_type in [10usize, 11] {
            let event: *mut AnyObject = msg_send![
                AnyClass::get(c"NSEvent").unwrap(),
                keyEventWithType: event_type,
                location: NSPoint::new(0., 0.),
                modifierFlags: 0usize,
                timestamp: 0f64,
                windowNumber: number,
                context: std::ptr::null_mut::<AnyObject>(),
                characters: &*characters,
                charactersIgnoringModifiers: &*characters,
                isARepeat: Bool::NO,
                keyCode: key_code
            ];
            assert!(!event.is_null());
            // Dispatch only to this view's actual GPUI key-event implementation.
            if event_type == 10 {
                let _: () = msg_send![view, keyDown: event];
            } else {
                let _: () = msg_send![view, keyUp: event];
            }
        }
    }
}

fn assert_hidden(app: *mut AnyObject, view: *mut AnyObject) {
    unsafe {
        let policy: isize = msg_send![app, activationPolicy];
        assert_eq!(policy, 2);
        let window: *mut AnyObject = msg_send![view, window];
        let visible: Bool = msg_send![window, isVisible];
        assert!(!visible.as_bool());
    }
}

async fn wait_for_value(
    webview: &gpui::Entity<Webview>,
    replies: &Rc<RefCell<Vec<String>>>,
    expected: &str,
    step: usize,
    cx: &mut gpui::AsyncApp,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let wanted = format!("{step}:{expected}");
    loop {
        let script = format!(
            "(()=>{{const e=document.getElementById('e');if(e){{e.focus();{}}}}})()",
            gpui_cef::post_message_script(&format!("{step} + ':' + e.value"))
        );
        webview.update(cx, |webview, _| webview.eval(&script));
        cx.background_executor()
            .timer(Duration::from_millis(30))
            .await;
        if replies.borrow().contains(&wanted) {
            break;
        }
        if Instant::now() >= deadline {
            let diagnostic = gpui_cef::post_message_script(
                "JSON.stringify({value:e.value,start:e.selectionStart,end:e.selectionEnd,active:document.activeElement.id,events:keyEvents})",
            );
            webview.update(cx, |webview, _| {
                webview.eval(&format!(
                    "(()=>{{const e=document.getElementById('e');{diagnostic}}})()"
                ))
            });
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            panic!(
                "step {step}: expected {wanted:?}, last reply {:?}",
                replies.borrow().last()
            );
        }
    }
}
