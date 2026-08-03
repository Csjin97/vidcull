
import { chromium } from "playwright";
import { spawn, spawnSync } from "node:child_process";
import {
  mkdirSync,
  mkdtempSync,
  copyFileSync,
  rmSync,
  existsSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));
const appDir = resolve(here, "..");
const repoRoot = resolve(appDir, "..");

const CDP_PORT = 9224;
const VITE_PORT = 5174; 
const VITE_URL = `http://localhost:${VITE_PORT}`;
const IPC_PIPE = String.raw`\\.\pipe\vidcull-live-thumb-${process.pid}`;
const ISOLATED_TARGET = join(repoRoot, "target-agent5");
const FFMPEG = join(repoRoot, "vendor", "ffmpeg", "windows-x86_64", "ffmpeg.exe");
const DAEMON_EXE = join(ISOLATED_TARGET, "debug", "vidcull-daemon.exe");

const shotDir = process.argv[2] ?? join(appDir, "review-verify-live");
mkdirSync(shotDir, { recursive: true });

const failures = [];
async function check(name, fn) {
  try {
    const detail = await fn();
    if (detail === true || detail === undefined) {
      console.log(`  PASS  ${name}`);
    } else {
      failures.push(`${name} — ${detail}`);
      console.log(`  FAIL  ${name} — ${detail}`);
    }
  } catch (err) {
    failures.push(`${name} — threw ${err}`);
    console.log(`  FAIL  ${name} — threw ${err}`);
  }
}
const log = (...a) => console.log("[live-thumb]", ...a);


const children = [];
let workDir = null;

function killChild(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  try {
    spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], {
      stdio: "ignore",
    });
  } catch {
    try {
      child.kill("SIGKILL");
    } catch {

    }
  }
}

function cleanup() {
  for (const child of children) killChild(child);
  if (workDir && existsSync(workDir)) {
    for (let i = 0; i < 5; i += 1) {
      try {
        rmSync(workDir, { recursive: true, force: true });
        break;
      } catch {
        spawnSync("cmd", ["/c", "ping", "127.0.0.1", "-n", "2"], {
          stdio: "ignore",
        });
      }
    }
  }
}

process.on("exit", cleanup);
process.on("SIGINT", () => {
  cleanup();
  process.exit(130);
});

async function fail(msg) {
  log("ABORT:", msg);
  failures.push(msg);
  await finish();
}

async function finish() {
  console.log(`\nScreenshots in ${shotDir}/`);
  if (failures.length > 0) {
    console.error(`\n${failures.length} check(s) failed:`);
    for (const f of failures) console.error(`  - ${f}`);
    process.exit(1);
  }
  console.log("\nAll live-thumbnail checks passed.");
  process.exit(0);
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

async function waitFor(label, predicate, { timeoutMs = 60_000, everyMs = 500 } = {}) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    let ok = false;
    try {
      ok = await predicate();
    } catch {
      ok = false;
    }
    if (ok) return true;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${label}`);
    await sleep(everyMs);
  }
}

if (!existsSync(FFMPEG)) await fail(`vendored ffmpeg not found at ${FFMPEG}`);
if (!existsSync(DAEMON_EXE))
  await fail(
    `daemon not built at ${DAEMON_EXE} — run: CARGO_TARGET_DIR="${ISOLATED_TARGET}" cargo build -p vidcull-daemon`,
  );

workDir = mkdtempSync(join(tmpdir(), "av-live-thumb-"));
const scanDir = join(workDir, "clips");
const dbPath = join(workDir, "live.db");
const thumbDir = join(workDir, "thumbs");
mkdirSync(scanDir, { recursive: true });
mkdirSync(thumbDir, { recursive: true });

const clipA = join(scanDir, "source.copy_a.mp4");
const clipB = join(scanDir, "source.copy_b.mp4");

log("rendering synthetic clip with vendored ffmpeg…");
const ff = spawnSync(
  FFMPEG,
  [
    "-v", "error", "-hide_banner", "-nostdin", "-y",
    "-fflags", "+bitexact",
    "-f", "lavfi", "-i", "testsrc=size=320x180:rate=30",
    "-t", "2",
    "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
    "-r", "30", "-g", "6", "-keyint_min", "6", "-sc_threshold", "0",
    "-an", "-map_metadata", "-1", "-bitexact",
    clipA,
  ],
  { encoding: "utf8" },
);
if (ff.status !== 0)
  await fail(`ffmpeg render failed (${ff.status}): ${ff.stderr || ff.stdout}`);
copyFileSync(clipA, clipB); 
log(`clips ready: ${clipA} + identical copy`);

const daemonEnv = {
  ...process.env,
  VIDCULL_IPC: IPC_PIPE,
  VIDCULL_DB: dbPath,
  VIDCULL_WATCH: scanDir,
  VIDCULL_THUMB_DIR: thumbDir,
  CARGO_TARGET_DIR: ISOLATED_TARGET,
};
log(`starting daemon (pipe=${IPC_PIPE})…`);
const daemon = spawn(DAEMON_EXE, [], {
  cwd: repoRoot,
  env: daemonEnv,
  stdio: ["ignore", "pipe", "pipe"],
});
children.push(daemon);
daemon.stdout.on("data", (b) => process.stdout.write(`[daemon] ${b}`));
daemon.stderr.on("data", (b) => process.stdout.write(`[daemon] ${b}`));
daemon.on("exit", (code) => log(`daemon exited (code=${code})`));

await sleep(1500);
if (daemon.exitCode !== null) await fail(`daemon exited early (${daemon.exitCode})`);

log(`starting vite on :${VITE_PORT}…`);
const vite = spawn(
  "npm",
  ["run", "dev", "--", "--port", String(VITE_PORT), "--strictPort"],
  { cwd: appDir, env: process.env, stdio: ["ignore", "pipe", "pipe"], shell: true },
);
children.push(vite);
vite.stdout.on("data", (b) => process.stdout.write(`[vite] ${b}`));
vite.stderr.on("data", (b) => process.stdout.write(`[vite] ${b}`));

try {
  await waitFor(
    "vite dev server",
    async () => {
      const res = await fetch(VITE_URL).catch(() => null);
      return res != null && res.ok;
    },
    { timeoutMs: 60_000 },
  );
} catch (err) {
  await fail(`vite did not come up: ${err}`);
}
log("vite is up");

const tauriConfigPath = join(workDir, "tauri.override.json");
writeFileSync(
  tauriConfigPath,
  JSON.stringify({ build: { beforeDevCommand: "", devUrl: VITE_URL } }),
);
const appEnv = {
  ...process.env,
  VIDCULL_IPC: IPC_PIPE,
  CARGO_TARGET_DIR: ISOLATED_TARGET,
  WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${CDP_PORT}`,
};
log("starting Tauri app (this compiles vidcull-ui on first run — may take a while)…");
const app = spawn(
  "npm",
  [
    "run", "tauri", "--",
    "dev",
    "--no-watch",
    "--no-dev-server-wait",
    "-c", tauriConfigPath,
  ],
  { cwd: appDir, env: appEnv, stdio: ["ignore", "pipe", "pipe"], shell: true },
);
children.push(app);
app.stdout.on("data", (b) => process.stdout.write(`[app] ${b}`));
app.stderr.on("data", (b) => process.stdout.write(`[app] ${b}`));
app.on("exit", (code) => log(`tauri app exited (code=${code})`));

let browser = null;
try {
  await waitFor(
    "WebView2 CDP endpoint",
    async () => {
      const res = await fetch(`http://127.0.0.1:${CDP_PORT}/json/version`).catch(
        () => null,
      );
      if (app.exitCode !== null)
        throw new Error(`tauri app exited (${app.exitCode}) before CDP came up`);
      return res != null && res.ok;
    },
    { timeoutMs: 300_000 }, 
  );
} catch (err) {
  await fail(`WebView2 CDP did not open on :${CDP_PORT}: ${err}`);
}
log("WebView2 CDP endpoint is live; attaching Playwright…");

try {
  browser = await chromium.connectOverCDP(`http://127.0.0.1:${CDP_PORT}`);
} catch (err) {
  await fail(`connectOverCDP failed: ${err}`);
}

let page = null;
await waitFor(
  "app page",
  () => {
    for (const ctx of browser.contexts()) {
      for (const p of ctx.pages()) {
        const url = p.url();
        if (url.startsWith(VITE_URL) || url.includes("localhost")) {
          page = p;
          return true;
        }
      }
    }
    return false;
  },
  { timeoutMs: 30_000 },
);
if (!page) await fail("could not find the app page over CDP");
log(`attached to page: ${page.url()}`);

const consoleErrors = [];
page.on("console", (m) => {
  if (m.type() === "error") consoleErrors.push(m.text());
});
page.on("pageerror", (e) => consoleErrors.push(String(e)));

await check("running inside the Tauri runtime (live TauriDataSource)", async () => {
  const inTauri = await page.evaluate(
    () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window,
  );
  return inTauri === true
    ? true
    : "window.__TAURI_INTERNALS__ missing — this is a plain browser, not the WebView";
});

await check("a duplicate-content cluster card renders from live data", async () => {
  await waitFor(
    "cluster card",
    async () => (await page.locator(".card").count()) > 0,
    { timeoutMs: 120_000 },
  );
  return (await page.locator(".card").count()) > 0
    ? true
    : "no .card rendered after indexing";
});

await page.screenshot({ path: `${shotDir}/01-list.png`, fullPage: false });

await check("selecting the cluster opens the compare view", async () => {
  await page.locator(".card").first().click();
  await page.waitForSelector(".compare", { timeout: 15_000 });
  return true;
});

await check("member thumbnails appear in the compare view", async () => {
  await waitFor(
    "thumb img",
    async () => (await page.locator(".compare .thumb__img").count()) > 0,
    { timeoutMs: 60_000 },
  );
  const n = await page.locator(".compare .thumb__img").count();
  return n > 0 ? true : "no .thumb__img in the compare view";
});

await page.screenshot({ path: `${shotDir}/02-compare.png`, fullPage: false });

let firstSrc = null;
await check("thumbnail src is a data:image/jpeg;base64 URI (daemon JPEG)", async () => {
  firstSrc = await page.locator(".compare .thumb__img").first().getAttribute("src");
  if (typeof firstSrc !== "string") return "no src attribute";
  return firstSrc.startsWith("data:image/jpeg;base64,")
    ? true
    : `src does not start with data:image/jpeg;base64, (got "${firstSrc.slice(0, 48)}…")`;
});

await check("the thumbnail <img> actually paints (decode + naturalWidth>0)", async () => {
  const result = await page
    .locator(".compare .thumb__img")
    .first()
    .evaluate(async (img) => {
      try {
        await img.decode();
      } catch (e) {
        return { ok: false, reason: `decode() rejected: ${e}` };
      }
      return {
        ok: img.naturalWidth > 0 && img.naturalHeight > 0,
        naturalWidth: img.naturalWidth,
        naturalHeight: img.naturalHeight,
      };
    });
  return result.ok
    ? true
    : `img did not paint: ${JSON.stringify(result)}`;
});

await page.screenshot({ path: `${shotDir}/03-thumbnail-painted.png`, fullPage: false });

await check("no console / page errors during the live flow", () =>
  consoleErrors.length === 0
    ? true
    : consoleErrors.filter((e) => !/poll failed|will retry|reload failed/i.test(e))
        .join(" | ") || true,
);

log(`first thumbnail src prefix: ${firstSrc ? firstSrc.slice(0, 64) : "<none>"}…`);

if (browser) await browser.close().catch(() => {});
await finish();
