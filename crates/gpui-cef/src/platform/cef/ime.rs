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

use cef::{ImplBrowserHost, Range as CefRange};
use gpui::{Bounds, Context, EntityInputHandler, Pixels, Point, UTF16Selection, Window};

use super::Webview;

impl Webview {
    /// Sends the text the IME has settled on, and forgets the composition.
    fn commit(&self, text: &str, replacement: Option<Range<usize>>) {
        self.shared.set_composition("");
        if let Some(host) = self.host() {
            host.ime_commit_text(
                Some(&text.into()),
                replacement.map(cef_range).as_ref(),
                // Relative to the end of the inserted text, which is where every
                // caller here wants the caret.
                0,
            );
        }
    }

    /// Sends the in-progress composition so the page can show it underlined.
    fn compose(
        &self,
        text: &str,
        replacement: Option<Range<usize>>,
        selection: Option<Range<usize>>,
    ) {
        self.shared.set_composition(text);
        if let Some(host) = self.host() {
            host.ime_set_composition(
                Some(&text.into()),
                // Passing no underlines lets Chromium apply its own, which is
                // what a page would get from a normal browser.
                None,
                replacement.map(cef_range).as_ref(),
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
        let composition = self.shared.composition();
        if composition.is_empty() {
            return None;
        }

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
        (length > 0).then_some(0..length)
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.shared.set_composition("");
        if let Some(host) = self.host() {
            host.ime_cancel_composition();
        }
    }

    /// Committed text: what the IME decided on, or a plain typed character.
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.commit(text, range);
    }

    /// Text still being composed, e.g. the kana before conversion.
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if new_text.is_empty() {
            // An empty composition means the IME gave up on it.
            self.unmark_text(_window, _cx);
            return;
        }
        self.compose(new_text, range, new_selected_range);
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

    #[test]
    fn ranges_convert_to_cef_ranges() {
        let range = cef_range(2..5);
        assert_eq!((range.from, range.to), (2, 5));
    }
}
