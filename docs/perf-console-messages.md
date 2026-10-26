# Decoding CEF console messages

Loaded DOCX save/recovery profiling in jade-elsa identified CEF console string
conversion on the native application's main thread, even with DevTools console
observation disabled. DOCX payloads travel as JSON containing base64 data.

Previously each console line was converted by CEF into a temporary UTF-8
allocation, formatted into a Rust `String`, and then copied again after removing
the bridge prefix. The handler now checks the prefix in the borrowed UTF-16
slice and converts only the payload directly into the event's owned `String`.
Ordinary console lines need no string conversion or allocation. The Windows
transport and public string protocol are unchanged. Malformed UTF-16 messages
are rejected, matching CEF's previous behavior.

The property tests cover arbitrary Unicode payloads, arbitrary UTF-16 units,
unrelated console lines, prefix placement, embedded NULs and surrogate pairs.
They do not impose a timing threshold on CI.

A standalone macOS comparison uses the actual CEF 151 conversion function as
the old implementation's oracle. It also checks 512 deterministic arbitrary
UTF-16 payloads against that implementation. Build and run it with a path to
an extracted CEF framework's binary:

```sh
rustc --edition=2021 -O docs/bench-console-message.rs -o /tmp/bench-console-message
/tmp/bench-console-message "$CEF_PATH/cef_macos_aarch64/Chromium Embedded Framework.framework/Chromium Embedded Framework"
```

It alternates old/new order for 25 pairs, discards four warmup pairs, and
reports median/p95 for 1 KiB and 11 MiB mostly-ASCII messages containing
Japanese text and an emoji. Allocation and destruction are included. These
are conversion measurements, not whole save or typing latency measurements;
whole-application behavior must be measured separately.

On an Apple Silicon Mac with CEF 151.3.24, the 11 MiB case measured
18.606/20.257 ms median/p95 before and 13.140/14.497 ms after. The 1 KiB
case rounded to 0.001 ms for both. This is one local run and includes
scheduling and allocator variance.

A separate release GPUI/CEF save comparison in jade-elsa alternated two
processes per implementation with the same local core WASM and SDK 0.0.53
JavaScript. Each process saved eight times and checked the complete body and
PNG bytes. Using only saves without DevTools console observation, excluding
two warmups per process, there were six measured samples per implementation.
Save medians were 128.753 ms before and 126.268 ms after; ranges were
111.185–139.250 and 117.695–131.970 ms. This small end-to-end difference is
inconclusive; the evidence supports reduced conversion cost and copying,
not a claim about perceptible save or typing speed.
