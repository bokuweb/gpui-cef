// End-to-end check of everything the webview hands to the application.
//
// Launches the bundled demo with GPUI_CEF_DEMO_AUTO=1, which answers every
// dialog, file chooser and download on the spot without showing anything,
// opens the feature test page, and clicks through it over CEF's remote
// debugging port. Each check looks at both ends of the round trip: the event
// the application logged, and what the page or the disk saw afterwards.
//
//   just e2e                    # bundles first
//   node examples/browser/e2e.mjs [path/to/gpui-cef-browser]
//
// Needs Node 22 or later (for the global WebSocket) and a desktop session:
// the demo opens a real window.

import { spawn } from "node:child_process";
import { existsSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const executable =
  process.argv[2] ?? "target/bundle/gpui-cef-browser.app/Contents/MacOS/gpui-cef-browser";
const port = 9229;
const demoDir = join(tmpdir(), "gpui-cef-demo");
const outDir = join(demoDir, "out");

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/// Rejects if `promise` takes longer than `ms`: a page held up by a dialog
/// nobody answers would otherwise hang the run.
function within(ms, what, promise) {
  let timer;
  const late = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${what} took longer than ${ms / 1000}s`)), ms);
  });
  return Promise.race([promise, late]).finally(() => clearTimeout(timer));
}

async function waitFor(what, check, timeout = 10_000) {
  const deadline = Date.now() + timeout;
  for (;;) {
    const value = await check();
    if (value) return value;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await sleep(100);
  }
}

/// The smallest CDP client that does the job.
class Cdp {
  static async connect(url) {
    const socket = new WebSocket(url);
    await new Promise((resolve, reject) => {
      socket.addEventListener("open", resolve, { once: true });
      socket.addEventListener("error", reject, { once: true });
    });
    return new Cdp(socket);
  }

  constructor(socket) {
    this.socket = socket;
    this.nextId = 1;
    this.pending = new Map();
    socket.addEventListener("message", (event) => {
      const message = JSON.parse(event.data);
      const pending = this.pending.get(message.id);
      if (!pending) return;
      this.pending.delete(message.id);
      if (message.error) pending.reject(new Error(message.error.message));
      else pending.resolve(message.result);
    });
    socket.addEventListener("close", () => {
      for (const pending of this.pending.values()) pending.reject(new Error("socket closed"));
      this.pending.clear();
    });
  }

  send(method, params = {}) {
    const id = this.nextId++;
    this.socket.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
  }

  async evaluate(expression) {
    const { result, exceptionDetails } = await within(10_000, "evaluating in the page", this.send("Runtime.evaluate", {
      expression,
      // Counts as a click: popups and file choosers need one.
      userGesture: true,
      awaitPromise: true,
      returnByValue: true,
    }));
    if (exceptionDetails) throw new Error(exceptionDetails.text);
    return result.value;
  }

  close() {
    this.socket.close();
  }
}

async function targets() {
  try {
    return await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
  } catch {
    return undefined;
  }
}

async function pageTarget() {
  return waitFor("the feature test page", async () => {
    if (appExited) throw new Error("the demo exited before its page came up");
    return (await targets())?.find((target) => target.type === "page" && target.url.endsWith("features.html"));
  }, 30_000);
}

// --- launch -----------------------------------------------------------------

if (!existsSync(executable)) {
  console.error(`no demo at ${executable}; run \`just bundle\` first`);
  process.exit(2);
}
// CEF keeps one browser process per profile, so a second demo would hand
// over to one already running — and that one answers to the port too.
if (await targets()) {
  console.error(`something is already listening on port ${port}; quit the running demo first`);
  process.exit(2);
}
rmSync(demoDir, { recursive: true, force: true });

let appLog = "";
const app = spawn(executable, [], {
  env: {
    ...process.env,
    GPUI_CEF_DEMO_AUTO: "1",
    GPUI_CEF_START_URL: "features",
    RUST_LOG: "gpui_cef_browser=info,gpui_cef=info",
  },
  stdio: ["ignore", "pipe", "pipe"],
});
app.stdout.on("data", (chunk) => (appLog += chunk));
app.stderr.on("data", (chunk) => (appLog += chunk));
let appExited = false;
app.on("exit", () => (appExited = true));

/// Event lines the demo logged, as `{event:?}`.
const events = () => appLog.split("\n").filter((line) => line.includes("webview event: "));
/// Waits for an event line that matches a regex, or contains every string in
/// an array.
const sawEvent = (pattern) => {
  const matches = Array.isArray(pattern)
    ? (line) => pattern.every((part) => line.includes(part))
    : (line) => pattern.test(line);
  return waitFor(`an event matching ${pattern}`, () => events().find(matches));
};

let cdp;
const connect = async () => {
  cdp?.close();
  cdp = await Cdp.connect((await pageTarget()).webSocketDebuggerUrl);
  await waitFor("the page to finish loading", () =>
    cdp.evaluate("document.readyState === 'complete' && !!document.getElementById('results')").catch(() => false),
  );
};
const results = () => cdp.evaluate("document.getElementById('results').textContent");
const pageSaid = (text) => waitFor(`the page to say ${JSON.stringify(text)}`, async () => (await results()).includes(text));
// Clicked from a timer so that a dialog the click opens does not hold up the
// evaluation that clicked it.
const click = (id) => cdp.evaluate(`setTimeout(() => document.getElementById(${JSON.stringify(id)}).click(), 0)`);
// A click as the user makes one, through the input pipeline rather than
// script. Chromium holds a page's second download until the user has
// interacted with it again, and a scripted click does not count.
const realClick = async (id) => {
  const { x, y } = await cdp.evaluate(`(() => {
    const element = document.getElementById(${JSON.stringify(id)});
    element.scrollIntoView({ block: "center" });
    const rect = element.getBoundingClientRect();
    return { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 };
  })()`);
  for (const type of ["mousePressed", "mouseReleased"]) {
    await cdp.send("Input.dispatchMouseEvent", { type, x, y, button: "left", clickCount: 1 });
  }
};
const fileText = (name) => waitFor(`${name} on disk`, () => existsSync(join(outDir, name)) && readFileSync(join(outDir, name)));

// --- checks -------------------------------------------------------------------

const checks = [
  ["window.open is blocked and reported", async () => {
    await click("window-open");
    await pageSaid("window.open returned null");
    await sawEvent(/PopupRequested \{ url: "https:\/\/example\.com\/", .*user_gesture: true/);
  }],
  ["a target=_blank rel=noopener link is blocked and reported", async () => {
    await click("blank-link");
    await sawEvent(/PopupRequested \{ url: "https:\/\/example\.org\/"/);
  }],
  ["the navigation filter keeps the page from leaving", async () => {
    await click("blocked-link");
    await sawEvent(/NavigationBlocked \{ url: "https:\/\/blocked\.example\/" \}/);
    await sleep(500);
    if (!(await cdp.evaluate("location.href")).endsWith("features.html")) throw new Error("the page navigated");
  }],
  ["alert() is answered by the application", async () => {
    await click("alert");
    await sawEvent(/JsDialog \{ id: DialogId\(\d+\), kind: Alert, message: "Hello from the page"/);
    await pageSaid("alert returned");
  }],
  ["confirm() returns the application's answer", async () => {
    await click("confirm");
    await sawEvent(/kind: Confirm, message: "Do you confirm\?"/);
    await pageSaid("confirm returned true");
  }],
  ["confirm() returns false when the application declines", async () => {
    await click("confirm-declined");
    await sawEvent(/kind: Confirm, message: "Please decline this one"/);
    await pageSaid("declined confirm returned false");
  }],
  ["prompt() returns the application's text", async () => {
    await click("prompt");
    await sawEvent(/kind: Prompt, message: "Your name\?", default_prompt: "Ada"/);
    await pageSaid('prompt returned "from gpui"');
  }],
  ["beforeunload asks the application before a reload", async () => {
    await click("beforeunload");
    await pageSaid("beforeunload on");
    await click("reload-from-app");
    await sawEvent(/kind: BeforeUnload \{ is_reload: true \}/);
    // Accepted, so the page reloads and starts its list over.
    await waitFor("the reload", async () => {
      try {
        const text = await results();
        return !text.includes("beforeunload on") && text.includes("loaded at");
      } catch {
        return false;
      }
    });
  }],
  ["<input type=file> gets the application's file", async () => {
    await click("file-one");
    await sawEvent(/FileDialog \{ id: FileDialogId\(\d+\), mode: Open,/);
    await pageSaid('file-one: picked.txt (first one says "Picked by the gpui-cef demo")');
  }],
  ["<input multiple accept> passes its filters on", async () => {
    await click("file-many");
    await sawEvent(['mode: OpenMultiple', '"image/*"', '".txt"', 'accept_extensions: [', '"png"', '"txt"']);
    await pageSaid("file-many: picked.txt");
  }],
  ["showSaveFilePicker() saves where the application says", async () => {
    await click("save-picker");
    await sawEvent(/mode: Save/);
    await pageSaid("save picker: wrote saved-by-page.txt");
    const text = String(await fileText("saved-by-page.txt"));
    if (text !== "Written through showSaveFilePicker\n") throw new Error(`wrote ${JSON.stringify(text)}`);
  }],
  ["a blob: download goes where the application says", async () => {
    await click("download-blob");
    await sawEvent(/DownloadRequested \{ id: DownloadId\(\d+\), url: "blob:.*", suggested_name: "from-blob\.txt", mime_type: "text\/plain"/);
    await sawEvent(/DownloadUpdated \{ id: DownloadId\(\d+\), state: Completed, .*from-blob\.txt/);
    const text = String(await fileText("from-blob.txt"));
    if (text !== "Hello from a blob the page built\n") throw new Error(`downloaded ${JSON.stringify(text)}`);
  }],
  ["a data: URL download goes where the application says", async () => {
    await realClick("download-data");
    await sawEvent(/suggested_name: "from-data-url\.txt"/);
    const text = String(await fileText("from-data-url.txt"));
    if (text !== "Hello from a data URL\n") throw new Error(`downloaded ${JSON.stringify(text)}`);
  }],
  ["a declined download is cancelled", async () => {
    await realClick("download-declined");
    const requested = await sawEvent(/DownloadRequested \{ id: (DownloadId\(\d+\)), .*suggested_name: "decline-me\.txt"/);
    const id = requested.match(/DownloadId\(\d+\)/)[0];
    await sawEvent([`DownloadUpdated { id: ${id}, state: Canceled`]);
    if (existsSync(join(outDir, "decline-me.txt"))) throw new Error("it was saved anyway");
  }],
  ["a frame that fails to load is reported", async () => {
    await click("load-error");
    await sawEvent(/LoadFailed \{ url: "http:\/\/does-not-exist\.invalid\/", error_code: -\d+, error_text: "ERR_[A-Z_]+", is_main_frame: false \}/);
  }],
  ["the page prints to PDF through the application", async () => {
    await click("print-pdf");
    await sawEvent(/PdfPrinted \{ path: ".*page\.pdf", ok: true \}/);
    const pdf = await fileText("page.pdf");
    if (pdf.subarray(0, 5).toString() !== "%PDF-") throw new Error("not a PDF");
  }],
  // Last among the file choosers: Chromium follows a folder choice with an
  // "upload these files?" confirmation that a windowless browser cannot show,
  // so the page never hears back, and it keeps one chooser at a time.
  ["<input webkitdirectory> reaches the application", async () => {
    await click("file-folder");
    await sawEvent(/FileDialog \{ id: FileDialogId\(\d+\), mode: OpenFolder/);
  }],
  ["a crashed renderer is reported, and reload brings the page back", async () => {
    await click("crash-renderer");
    await sawEvent(/RenderProcessTerminated \{ status: (Crashed|Abnormal|Killed)/);
    // The demo reloads on its own in automatic mode.
    await connect();
    await pageSaid("loaded at");
  }],
];

let failed = 0;
try {
  await connect();
  for (const [name, check] of checks) {
    try {
      await within(45_000, "the check", check());
      console.log(`ok   ${name}`);
    } catch (error) {
      failed++;
      console.log(`FAIL ${name}: ${error.message}`);
    }
    if (appExited) throw new Error("the demo exited");
  }
} catch (error) {
  failed++;
  console.log(`FAIL ${error.message}`);
} finally {
  cdp?.close();
  // Chromium handles SIGTERM by shutting down through its message loop,
  // which a modal panel can hold up; make sure the demo goes.
  app.kill();
  await sleep(2_000);
  if (!appExited) app.kill("SIGKILL");
}

if (failed > 0 || process.env.E2E_VERBOSE) {
  console.log("\nevents the demo logged:\n" + events().join("\n"));
}
console.log(failed === 0 ? `\nall ${checks.length} checks passed` : `\n${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
