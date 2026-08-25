//! Task runner that assembles the macOS `.app` bundle.
//!
//! CEF looks for `Chromium Embedded Framework.framework` under
//! `.app/Contents/Frameworks/` and launches its child processes as
//! `<name> Helper.app`. A bare executable built by `cargo run` therefore cannot
//! start. This calls into cef-rs's `build_util` to produce that layout.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("bundling is only needed on macOS");
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use cef::build_util::mac::{build_bundle, BundleInfo};

    const EXECUTABLE: &str = "gpui-cef-browser";

    let out_dir = std::path::PathBuf::from("target/bundle");
    std::fs::create_dir_all(&out_dir)?;

    let info = BundleInfo::new(
        EXECUTABLE,
        "dev.bokuweb.gpui-cef-browser",
        "gpui-cef browser",
        "en",
        semver::Version::new(0, 1, 0),
    );

    match build_bundle(&out_dir, EXECUTABLE, info) {
        Ok(app) => {
            println!("{}", app.display());
            Ok(())
        }
        Err(err) => Err(anyhow::anyhow!("failed to bundle the app: {err:?}")),
    }
}
