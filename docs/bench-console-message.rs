use std::{
    ffi::{c_char, c_void, CString},
    hint::black_box,
    time::Instant,
};
#[repr(C)]
struct CefString {
    ptr: *mut u8,
    len: usize,
    dtor: Option<unsafe extern "C" fn(*mut u8)>,
}
unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
    fn dlsym(h: *mut c_void, name: *const c_char) -> *mut c_void;
}
type Convert = unsafe extern "C" fn(*const u16, usize, *mut CefString) -> i32;
type Clear = unsafe extern "C" fn(*mut CefString);
fn old(line: &[u16], convert: Convert, clear: Clear) -> Option<String> {
    unsafe {
        let mut out: CefString = std::mem::zeroed();
        let ok = convert(line.as_ptr(), line.len(), &mut out);
        let text = if ok != 0 {
            std::str::from_utf8(std::slice::from_raw_parts(out.ptr, out.len))
                .unwrap()
                .to_string()
        } else {
            String::new()
        };
        clear(&mut out);
        text.strip_prefix("gpui-cef:message:").map(str::to_owned)
    }
}
#[path = "../crates/gpui-cef/src/message.rs"]
mod message;
pub const MESSAGE_PREFIX: &str = "gpui-cef:message:";
fn main() {
    unsafe {
        let path = CString::new(std::env::args().nth(1).unwrap()).unwrap();
        let handle = dlopen(path.as_ptr(), 2);
        assert!(!handle.is_null());
        let convert: Convert =
            std::mem::transmute(dlsym(handle, c"cef_string_utf16_to_utf8".as_ptr()));
        let clear: Clear = std::mem::transmute(dlsym(handle, c"cef_string_utf8_clear".as_ptr()));
        let mut malformed: Vec<u16> = MESSAGE_PREFIX.encode_utf16().collect();
        malformed.push(0xd800);
        println!(
            "malformed old={:?} new={:?}",
            old(&malformed, convert, clear),
            message::message_payload_utf16(&malformed)
        );
        let mut rng = 20261026u64;
        for n in 0..512 {
            let mut line: Vec<u16> = MESSAGE_PREFIX.encode_utf16().collect();
            for _ in 0..n {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                line.push((rng >> 32) as u16);
            }
            assert_eq!(
                old(&line, convert, clear),
                message::message_payload_utf16(&line)
            );
        }
        println!("512 arbitrary UTF-16 payloads match CEF 151");
        for size in [1024usize, 11 * 1024 * 1024] {
            let payload = "A".repeat(size) + "日本語😀";
            let line: Vec<u16> = (MESSAGE_PREFIX.to_owned() + &payload)
                .encode_utf16()
                .collect();
            assert_eq!(
                old(&line, convert, clear).as_deref(),
                Some(payload.as_str())
            );
            assert_eq!(
                message::message_payload_utf16(&line).as_deref(),
                Some(payload.as_str())
            );
            let mut a = vec![];
            let mut b = vec![];
            for i in 0..25 {
                for new in if i % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                } {
                    let t = Instant::now();
                    let value = if new {
                        message::message_payload_utf16(black_box(&line))
                    } else {
                        old(black_box(&line), convert, clear)
                    };
                    black_box(value);
                    if i >= 4 {
                        if new { &mut b } else { &mut a }.push(t.elapsed().as_secs_f64() * 1000.);
                    }
                }
            }
            a.sort_by(f64::total_cmp);
            b.sort_by(f64::total_cmp);
            println!(
                "{size} bytes old median={:.3} p95={:.3} ms new median={:.3} p95={:.3} ms",
                a[a.len() / 2],
                a[(a.len() - 1) * 95 / 100],
                b[b.len() / 2],
                b[(b.len() - 1) * 95 / 100]
            );
        }
    }
}
