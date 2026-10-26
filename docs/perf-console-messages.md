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
