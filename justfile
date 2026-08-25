# Build tasks for gpui-cef.
#
# Two environment variables have to be set on every build:
#   CEF_PATH — where the CEF binaries live. cef-dll-sys downloads them (~500MB)
#              if they are missing.
#   CXXFLAGS — a workaround for installations where the Command Line Tools libc++
#              headers are broken. A stale
#              /Library/Developer/CommandLineTools/usr/include/c++/v1 shadows the
#              complete libc++ in the SDK, and libcef_dll_wrapper then fails to
#              build with "<atomic> not found". Reinstalling the Command Line
#              Tools makes this unnecessary.

sdk := `xcrun --show-sdk-path 2>/dev/null || echo ""`

export CEF_PATH := env_var("HOME") / ".local/share/cef"
export CXXFLAGS := "-nostdinc++ -isystem " + sdk + "/usr/include/c++/v1"

# Check that the prerequisites are in place
doctor:
    @echo "xcode-select: $(xcode-select -p)"
    @xcrun -f metal >/dev/null 2>&1 \
      && echo "metal:       ok" \
      || echo "metal:       not found (only needed without gpui's runtime_shaders feature)"
    @test -f "{{sdk}}/usr/include/c++/v1/atomic" \
      && echo "libc++:      ok ({{sdk}})" \
      || echo "libc++:      NOT FOUND in the SDK"
    @command -v ninja >/dev/null && echo "ninja:       ok" || echo "ninja:       missing — brew install ninja"
    @command -v cmake >/dev/null && echo "cmake:       ok" || echo "cmake:       missing — brew install cmake"

check:
    cargo check --workspace

build:
    cargo build -p gpui-cef-browser

# Assemble the .app bundle. CEF finds its framework and helper processes through
# the bundle layout, so this is not optional on macOS.
bundle:
    cargo run -p xtask

# Bundle and launch
run: bundle
    open target/bundle/gpui-cef-browser.app

fmt:
    cargo fmt --all

clippy:
    cargo clippy --workspace --all-targets
