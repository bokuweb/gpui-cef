//! Bridges the OS input method to CEF, so text can be typed into the page.
//!
//! # Why this is not just "forward the keystrokes"
//!
//! Off-screen rendering means CEF has no window for the OS to attach an input
//! context to, so Chromium never sees the IME. Sending key events alone gets you
//! ASCII and nothing else: Japanese, Chinese, Korean, and dead-key accents all
//! arrive as *composition*, which the OS negotiates with the focused view rather
//! than delivering as keystrokes.
//!
//! gpui models that negotiation with [`EntityInputHandler`], which is written for
//! an editor that owns its text. Here the page owns the text and we cannot see
//! it. That turns out to be workable, because the OS only ever asks about the
//! text it is currently composing: [`Shared`] keeps a mirror of exactly that much
//! and CEF reports where it landed through `OnImeCompositionRangeChanged`.
//!
//! # Offsets
//!
//! Every range here is in **UTF-16 code units**, both in gpui and in CEF's
//! `cef_range_t` on macOS, so they map across directly. They do *not* map to Rust
//! byte offsets, hence [`utf16_slice`] rather than plain slicing.

use std::ops::Range;

use cef::{CompositionUnderline, ImplBrowserHost, Range as CefRange};
use gpui::{Bounds, Context, EntityInputHandler, Pixels, Point, UTF16Selection, Window};

use super::Webview;

impl Webview {
    /// Sends text after AppKit has finished its current input-client callback.
    fn commit_now(&self, text: &str, was_composing: bool, pending_key: Option<&gpui::Keystroke>) {
        super::input_trace(format_args!(
            "commit_now text={text:?} was_composing={was_composing} pending_key={pending_key:?}"
        ));
        if let Some(host) = self.host() {
            if was_composing {
                // The ranges AppKit gives gpui refer to our small composition
                // mirror, not to the page's document. CEF already owns the real
                // selection, so an absent replacement range commits at its caret.
                let replacement_range = invalid_cef_range();
                host.ime_commit_text(Some(&text.into()), Some(&replacement_range), 0);
            } else {
                // ImeCommitText completes an existing composition; it is not
                // Chromium's general text insertion API. Plain text therefore
                // travels as CHAR events after AppKit accepts it.
                if let Some(keystroke) = pending_key {
                    host.send_key_event(Some(&super::input::key_down_event(keystroke)));
                }
                for event in super::input::text_events(text) {
                    host.send_key_event(Some(&event));
                }
            }
        }
    }

    /// Sends the in-progress composition so the page can show it underlined.
    fn compose_now(&self, text: &str, selection: Option<Range<usize>>) {
        super::input_trace(format_args!(
            "compose_now text={text:?} selection={selection:?}"
        ));
        if let Some(host) = self.host() {
            // Match cefclient's macOS OSR implementation. Even when AppKit
            // supplies an un-attributed string it creates a default underline
            // spanning the whole composition instead of passing a null list.
            let underlines = [CompositionUnderline {
                range: CefRange {
                    from: 0,
                    to: utf16_len(text) as u32,
                },
                color: 0xff00_0000,
                ..Default::default()
            }];
            // AppKit reports NSNotFound when no document range should be
            // replaced. cefclient preserves that as UINT_MAX rather than a
            // null pointer, which is significant to Chromium's macOS path.
            let replacement_range = invalid_cef_range();
            host.ime_set_composition(
                Some(&text.into()),
                Some(&underlines),
                // GPUI's range is relative to the mirrored composition and
                // cannot be used as a document range in Chromium.
                Some(&replacement_range),
                selection.map(cef_range).as_ref(),
            );
        }
    }
}

impl EntityInputHandler for Webview {
    /// The OS only asks about text it is composing; anything else lives in the
    /// page and is not reachable from here.
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        super::input_trace(format_args!("text_for_range range={range:?}"));
        let composition = self.shared.composition();
        let length = utf16_len(&composition);
        let clamped = range.start.min(length)..range.end.min(length);
        if clamped != range {
            *adjusted_range = Some(clamped.clone());
        }
        Some(utf16_slice(&composition, clamped))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        // The page's real selection is not visible from here. Reporting an empty
        // one at the end of the composition is enough for the IME to proceed, and
        // returning None would make macOS treat the view as not accepting text.
        let caret = utf16_len(&self.shared.composition());
        super::input_trace(format_args!("selected_text_range caret={caret}"));
        Some(UTF16Selection {
            range: caret..caret,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let length = utf16_len(&self.shared.composition());
        super::input_trace(format_args!("marked_text_range length={length}"));
        (length > 0).then_some(0..length)
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        super::input_trace("unmark_text");
        self.shared.set_composition("");
        self.pending_printable_key = None;
        cx.defer_in(window, |this, _, _| {
            if let Some(host) = this.host() {
                host.ime_finish_composing_text(0);
            }
        });
    }

    /// Committed text: what the IME decided on, or a plain typed character.
    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_composing = !self.shared.composition().is_empty();
        super::input_trace(format_args!(
            "replace_text text={text:?} range={_range:?} was_composing={was_composing}"
        ));
        self.shared.set_composition("");
        let pending_key = if was_composing {
            None
        } else {
            self.pending_printable_key.take()
        };
        self.pending_printable_key = None;
        let text = text.to_owned();
        cx.defer_in(window, move |this, _, _| {
            this.commit_now(&text, was_composing, pending_key.as_ref())
        });
    }

    /// Text still being composed, e.g. the kana before conversion.
    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        super::input_trace(format_args!(
            "set_marked_text text={new_text:?} range={_range:?} selection={new_selected_range:?}"
        ));
        if new_text.is_empty() {
            // Input methods use an empty marked string to cancel, which is
            // distinct from NSTextInputClient::unmarkText (finish and keep).
            self.shared.set_composition("");
            self.pending_printable_key = None;
            cx.defer_in(window, |this, _, _| {
                if let Some(host) = this.host() {
                    host.ime_cancel_composition();
                }
            });
            return;
        }
        self.shared.set_composition(new_text);
        self.pending_printable_key = None;
        let text = new_text.to_owned();
        cx.defer_in(window, move |this, _, _| {
            this.compose_now(&text, new_selected_range)
        });
    }

    /// Where to put the candidate window. CEF reports the composition rects in
    /// the webview's own space, so they need the element's origin added back.
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        super::input_trace(format_args!("bounds_for_range range={range_utf16:?}"));
        let mut bounds = self.shared.composition_bounds(range_utf16)?;
        bounds.origin += element_bounds.origin;
        Some(bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        // Used for dictionary lookup by pointing at text. Answering it would mean
        // hit-testing inside the page, which CEF does not expose.
        None
    }
}

fn cef_range(range: Range<usize>) -> CefRange {
    CefRange {
        from: range.start as u32,
        to: range.end as u32,
    }
}

fn invalid_cef_range() -> CefRange {
    CefRange {
        from: u32::MAX,
        to: u32::MAX,
    }
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Slices by UTF-16 code unit, which is what both gpui and CEF count in.
fn utf16_slice(text: &str, range: Range<usize>) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let end = range.end.min(units.len());
    let start = range.start.min(end);
    String::from_utf16_lossy(&units[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_length_counts_code_units_not_characters() {
        assert_eq!(utf16_len("abc"), 3);
        // Japanese stays in the basic plane: one unit each.
        assert_eq!(utf16_len("にほんご"), 4);
        // Astral characters take two, which is what the OS counts.
        assert_eq!(utf16_len("\u{1F600}"), 2);
    }

    #[test]
    fn utf16_slice_uses_the_same_units_the_os_does() {
        assert_eq!(utf16_slice("にほんご", 1..3), "ほん");
        assert_eq!(utf16_slice("abc", 0..2), "ab");
    }

    #[test]
    fn invalid_range_matches_cef_macos_ns_not_found() {
        let range = invalid_cef_range();
        assert_eq!(range.from, u32::MAX);
        assert_eq!(range.to, u32::MAX);
    }

    #[test]
    fn out_of_range_slices_clamp_instead_of_panicking() {
        // The OS can ask about a range that no longer exists after a fast edit.
        assert_eq!(utf16_slice("abc", 1..99), "bc");
        assert_eq!(utf16_slice("abc", 99..99), "");
        assert_eq!(utf16_slice("", 0..5), "");
    }

    #[test]
    fn slicing_an_astral_pair_in_half_does_not_panic() {
        // Splitting a surrogate pair is invalid UTF-16; lossy conversion keeps
        // this from taking the process down.
        assert_eq!(utf16_slice("\u{1F600}", 0..1).chars().count(), 1);
    }
}
