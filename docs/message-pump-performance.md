# macOS external message-pump latency

Measured on 2026-10-25, Apple M1 Pro, macOS 27.0.1, debug browser example.
Baseline: main `10fb1fe5742abe941d6100573a297cebb4cd4872`, with only the same
benchmark instrumentation added. Candidate: scheduling callback, main run-loop
source, reentrancy guard and accelerated-frame cadence limit in this change.
Other development workloads were running; these are local observations, not a
latency guarantee. The largest candidate outlier exceeded the baseline.

| Run | Baseline median / p95 / max (ms) | Candidate median / p95 / max (ms) |
| --- | --- | --- |
| 1 | 15.366 / 20.158 / 20.630 | 0.235 / 4.273 / 11.459 |
| 2 | 15.292 / 17.913 / 22.146 | 1.085 / 8.907 / 48.789 |
| 3 | 15.986 / 26.101 / 32.104 | 0.459 / 5.212 / 13.390 |

## Reproduce

Build the browser bundle using `cargo run --locked -p xtask`, then run
`node examples/browser/input-e2e.mjs` three times on each revision. Supply the
same CEF and C++ SDK environment used by the regular macOS build. The script
copies and signs its own background-only bundle. It does not activate a window,
use a human profile, or send OS keyboard events.

Each run sends 110 sequential eval requests that immediately post a message
back to the host. Timing starts immediately before eval and ends inside the
native message callback, excluding executor polling. Discard 10 warmups; sort
the remaining 100 samples and use nearest-rank median (sample 50) and p95
(sample 95). The JSON result includes every sample and the summary.

This tests host/renderer IPC scheduling, **not** physical keyboard/IME handling,
DOCX editing, layout, paint or presentation latency, or audible feedback. The
same run retains the 24-step native input, Unicode and menu-replay assertions.
Timing is reported rather than used as a flaky CI threshold. Unit/property
tests enforce callback delivery, deadline replacement, fallback bounds,
recursive tick deferral and the accelerated-frame limit.

## Scheduling contract

CEF's [browser process handler](https://github.com/chromiumembedded/cef/blob/master/include/cef_browser_process_handler.h)
allows scheduling from any thread and requires immediate work to run reasonably
soon, delayed requests to replace pending deadlines, and a fallback when no
requests arrive. The callback only records a bounded deadline and signals a
process-lifetime CFRunLoopSource. The main-thread source rearms the timer; it
never runs CEF synchronously. The 60Hz timer remains as the fallback.
