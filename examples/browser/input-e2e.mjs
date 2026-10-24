// Exercises the actual AppKit -> GPUI -> CEF text client in a disposable,
// background-only bundle. No OS keyboard events, activation, shared profile,
// remote debugging port, or visible window. This does not test a real IME.
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { cpSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

assert.equal(process.platform, "darwin");
const source = resolve(process.argv[2] ?? "target/bundle/gpui-cef-browser.app");
const directory = mkdtempSync(join(tmpdir(), "gpui-cef-input-"));
const app = join(directory, "Input Probe.app");
let child;
try {
  cpSync(source, app, { recursive: true, dereference: false });
  const plist = join(app, "Contents/Info.plist");
  execFileSync("/usr/libexec/PlistBuddy", ["-c", "Add :LSBackgroundOnly bool true", plist]);
  execFileSync("/usr/libexec/PlistBuddy", ["-c", "Set :CFBundleIdentifier dev.bokuweb.gpui-cef.input-probe", plist]);
  execFileSync("codesign", ["--force", "--deep", "--sign", "-", app], { stdio: "pipe" });
  const report = join(directory, "report.json");
  child = spawn(join(app, "Contents/MacOS/gpui-cef-browser"), [], {
    env: { ...process.env, GPUI_CEF_INPUT_PROBE_REPORT: report },
    stdio: ["ignore", "pipe", "pipe"], detached: true,
  });
  let output = "";
  for (const stream of [child.stdout, child.stderr]) {
    stream.on("data", (data) => { output = (output + data).slice(-16000); });
  }
  const timer = setTimeout(() => {
    try { process.kill(-child.pid, "SIGKILL"); } catch {}
  }, 45000);
  const [code, signal] = await new Promise((resolve, reject) => {
    child.on("error", reject);
    child.on("exit", (code, signal) => resolve([code, signal]));
  }).finally(() => clearTimeout(timer));
  assert.equal(code, 0, `probe exited ${code}/${signal}\n${output}`);
  const result = JSON.parse(readFileSync(report, "utf8"));
  assert.equal(result.visible, false);
  assert.equal(result.activationPolicy, 2);
  assert.deepEqual(result.values, [
    "Aé😀", "Aé😀に", "Aé😀日本😀", "Aé😀日本😀", "Aé😀日本😀!",
    "Aé😀日本😀!語", "Aé😀日本😀!語", "Aé😀日本😀!語消", "Aé😀日本😀!語",
  ]);
  assert.equal(result.realKeyboardImeOrAudioVerified, false);
  console.log(JSON.stringify(result, null, 2));
} finally {
  // Only this test's process group and temporary bundle can be removed.
  if (child?.exitCode === null && child?.signalCode === null) {
    try { process.kill(-child.pid, "SIGKILL"); } catch {}
  }
  rmSync(directory, { recursive: true, force: true });
}
