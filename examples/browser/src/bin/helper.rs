//! The binary CEF spawns for its child processes (renderer, GPU, utility, ...).
//!
//! macOS requires those to be a separate executable, shipped inside the `.app`
//! as `Frameworks/<name> Helper.app`. `bundle-cef-app` reads
//! `package.metadata.cef.bundle.helper_name` and puts this binary there.
//!
//! Only macOS uses CEF, so on every other platform this is an empty program that
//! exists to keep the target list identical across platforms.

#[cfg(target_os = "macos")]
fn main() {
    use cef::{api_hash, args::Args, execute_process, library_loader, sys, App};

    let args = Args::new();

    // cef-dll-sys is built with the sandbox enabled (USE_SANDBOX=ON) by
    // default. Skipping this call makes the GPU and network child processes die
    // immediately with exit_code=5, and then nothing loads and nothing paints.
    let _sandbox = {
        let mut sandbox = cef::sandbox::Sandbox::new();
        sandbox.initialize(args.as_main_args());
        sandbox
    };

    // The helper sits three levels below Frameworks/, hence `helper = true`.
    let _loader = {
        let loader = library_loader::LibraryLoader::new(&std::env::current_exe().unwrap(), true);
        assert!(loader.load(), "failed to load the CEF framework");
        loader
    };

    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    execute_process(
        Some(args.as_main_args()),
        None::<&mut App>,
        std::ptr::null_mut(),
    );
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("this helper is only used by CEF on macOS");
}
