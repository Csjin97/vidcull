
import { chromium } from "playwright";
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, existsSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));
const appDir = resolve(here, "..");
const repoRoot = resolve(appDir, "..");

const CDP_PORT = 9228; 
const VITE_PORT = 5178; 
const VITE_URL = `http://localhost:${VITE_PORT}`;
const IPC_PIPE = String.raw`\\.\pipe\vidcull-live-tray-${process.pid}`;
const ISOLATED_TARGET = join(repoRoot, "target-agent7");
const DAEMON_EXE = join(ISOLATED_TARGET, "debug", "vidcull-daemon.exe");

const shotDir = process.argv[2] ?? join(appDir, "review-verify-live-tray");
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
const log = (...a) => console.log("[live-tray]", ...a);


const children = [];
let workDir = null;
let appOutput = "";

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
  console.log("\nAll live-tray checks passed.");
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

if (process.platform !== "win32")
  await fail("this verification is Windows-only (WebView2 + tray)");
if (!existsSync(DAEMON_EXE))
  await fail(
    `daemon not built at ${DAEMON_EXE} — run: CARGO_TARGET_DIR="${ISOLATED_TARGET}" cargo build -p vidcull-daemon`,
  );

workDir = mkdtempSync(join(tmpdir(), "av-live-tray-"));
const dbPath = join(workDir, "live.db");

const daemonEnv = {
  ...process.env,
  VIDCULL_IPC: IPC_PIPE,
  VIDCULL_DB: dbPath,
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
  ["run", "tauri", "--", "dev", "--no-watch", "--no-dev-server-wait", "-c", tauriConfigPath],
  { cwd: appDir, env: appEnv, stdio: ["ignore", "pipe", "pipe"], shell: true },
);
children.push(app);
app.stdout.on("data", (b) => {
  appOutput += b;
  process.stdout.write(`[app] ${b}`);
});
app.stderr.on("data", (b) => {
  appOutput += b;
  process.stdout.write(`[app] ${b}`);
});
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

async function windowIsVisible() {
  return page.evaluate(() =>
    window.__TAURI__.window.getCurrentWindow().isVisible(),
  );
}
async function closeWindow() {
  return page.evaluate(() =>
    window.__TAURI__.window.getCurrentWindow().close(),
  );
}
async function showWindow() {
  return page.evaluate(async () => {
    const w = window.__TAURI__.window.getCurrentWindow();
    await w.show();
    await w.setFocus();
    return w.isVisible();
  });
}

await check("running inside the Tauri runtime (live, not a plain browser)", async () => {
  const inTauri = await page.evaluate(
    () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window,
  );
  return inTauri === true
    ? true
    : "window.__TAURI_INTERNALS__ missing — this is a plain browser, not the WebView";
});

await check("the main window starts visible", async () => {
  await waitFor("window visible", async () => (await windowIsVisible()) === true, {
    timeoutMs: 20_000,
  });
  return (await windowIsVisible()) === true ? true : "window did not report visible";
});
await page.screenshot({ path: `${shotDir}/01-visible.png`, fullPage: false });

await check("the daemon reports background_enabled = true (hide-to-tray branch)", async () => {
  const enabled = await page.evaluate(async () => {
    const s = await window.__TAURI__.core.invoke("get_settings");
    return s?.background_enabled;
  });
  return enabled === true
    ? true
    : `background_enabled = ${enabled} — expected true so close hides to tray`;
});

await check("closing the window HIDES it while the WebView process SURVIVES", async () => {
  await closeWindow();
  await waitFor("window hidden", async () => (await windowIsVisible()) === false, {
    timeoutMs: 15_000,
  });
  if (await windowIsVisible()) return "window still visible after close — did not hide to tray";
  const alive = await page.evaluate(() => 21 * 2).catch(() => null);
  if (alive !== 42) return "WebView page no longer responds — the app quit instead of hiding";
  if (app.exitCode !== null) return `tauri app exited (${app.exitCode}) on close — it should survive`;
  return true;
});
await page.screenshot({ path: `${shotDir}/02-hidden.png`, fullPage: false }).catch(() => {});

await check("the window can be RESTORED from the tray (열기 path: show + focus)", async () => {
  const visible = await showWindow();
  await waitFor("window visible again", async () => (await windowIsVisible()) === true, {
    timeoutMs: 15_000,
  });
  return visible === true && (await windowIsVisible()) === true
    ? true
    : "window did not come back visible after the restore path";
});
await page.screenshot({ path: `${shotDir}/03-restored.png`, fullPage: false });

await check("the system tray icon was created (no setup fallback line logged)", async () => {
  return /could not create system tray/.test(appOutput)
    ? "app logged 'could not create system tray' — tray::build_tray failed on this host"
    : true;
});

await check("no fatal console / page errors during the tray flow", () =>
  consoleErrors.length === 0
    ? true
    : consoleErrors.filter((e) => !/poll failed|will retry|reload failed/i.test(e))
        .join(" | ") || true,
);

if (browser) await browser.close().catch(() => {});
await finish();
