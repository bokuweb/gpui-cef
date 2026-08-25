//! Backend selection. `init` / `Runtime` / `Webview` are swapped per platform.

#[cfg(target_os = "macos")]
#[path = "cef/mod.rs"]
mod imp;

#[cfg(target_os = "windows")]
#[path = "wry/mod.rs"]
mod imp;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[path = "unsupported.rs"]
mod imp;

pub use imp::{init, Runtime, Webview};
