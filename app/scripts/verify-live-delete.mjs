
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

const CDP_PORT = 9226; 
const VITE_PORT = 5176; 
const VITE_URL = `http://localhost:${VITE_PORT}`;
const IPC_PIPE = String.raw`\\.\pipe\vidcull-live-delete-${process.pid}`;
const ISOLATED_TARGET = join(repoRoot, "target-agent6");
const FFMPEG = join(repoRoot, "vendor", "ffmpeg", "windows-x86_64", "ffmpeg.exe");
const DAEMON_EXE = join(ISOLATED_TARGET, "debug", "vidcull-daemon.exe");

const shotDir = process.argv[2] ?? join(appDir, "review-verify-live-delete");
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
const log = (...a) => console.log("[live-delete]", ...a);


const children = [];
let workDir = null;
const trashedToPurge = new Set();

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
  for (const original of trashedToPurge) {
    try {
      purgeFromRecycleBin(original);
    } catch {

    }
  }
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
  console.log("\nAll live-delete checks passed.");
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

async function forceUiReload() {
  const tabs = page.locator(".review__tabs .tab");
  if ((await tabs.count()) < 2) return;
  await tabs.nth(1).click(); 
  await sleep(400);
  await tabs.nth(0).click(); 
  await sleep(400);
}



function powershell(script) {
  return spawnSync(
    "powershell",
    ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script],
    { encoding: "utf8" },
  );
}

function psLit(s) {
  return `'${String(s).replace(/'/g, "''")}'`;
}

const PS_FIND_ITEM = String.raw`
function Norm([string]$p) { if ($null -eq $p) { return '' } ($p -replace '/','\').TrimEnd('\').ToLowerInvariant() }
function Find-RecycleItem([string]$target) {
  $want = Norm $target
  $shell = New-Object -ComObject Shell.Application
  $bin = $shell.Namespace(0xA)
  foreach ($it in @($bin.Items())) {
    $from = $null
    try { $from = $it.ExtendedProperty('System.Recycle.DeletedFrom') } catch { $from = $null }
    if (-not $from) { continue }
    $orig = Norm (Join-Path $from $it.Name)
    if ($orig -eq $want) { return $it }
  }
  return $null
}
`;


function recycleBinHasPath(original) {
  const script =
    PS_FIND_ITEM +
    `$t = ${psLit(original)};` +
    `$item = Find-RecycleItem $t;` +
    `if ($item) { 'FOUND' } else { 'MISSING' }`;
  const r = powershell(script);
  return /FOUND/.test(r.stdout || "");
}


function restoreFromRecycleBin(original) {
  const script =
    PS_FIND_ITEM +
    `$t = ${psLit(original)};` +
    `$item = Find-RecycleItem $t;` +
    `if (-not $item) { 'NO_ITEM'; exit 0 }` +
    `$verb = $item.Verbs() | Where-Object { $_.Name -match 'undelete|restore|복원|Restore' } | Select-Object -First 1;` +
    `if ($verb) { $verb.DoIt(); 'RESTORED' } else { 'NO_VERB' }`;
  const r = powershell(script);
  return /RESTORED/.test(r.stdout || "");
}


function purgeFromRecycleBin(original) {
  const script =
    PS_FIND_ITEM +
    `$t = ${psLit(original)};` +
    `$item = Find-RecycleItem $t;` +
    `if ($item) { Remove-Item -LiteralPath $item.Path -Force -Recurse -ErrorAction SilentlyContinue; 'PURGED' } else { 'ABSENT' }`;
  powershell(script);
}

if (process.platform !== "win32")
  await fail("this verification is Windows-only (real Recycle Bin round trip)");
if (!existsSync(FFMPEG)) await fail(`vendored ffmpeg not found at ${FFMPEG}`);
if (!existsSync(DAEMON_EXE))
  await fail(
    `daemon not built at ${DAEMON_EXE} — run: CARGO_TARGET_DIR="${ISOLATED_TARGET}" cargo build -p vidcull-daemon`,
  );

workDir = mkdtempSync(join(tmpdir(), "av-live-delete-"));
const scanDir = join(workDir, "clips");
const dbPath = join(workDir, "live.db");
const thumbDir = join(workDir, "thumbs");
mkdirSync(scanDir, { recursive: true });
mkdirSync(thumbDir, { recursive: true });

const clipKeep = join(scanDir, "source.keep.mp4"); 
const clipDel = join(scanDir, "source.dupe.mp4"); 

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
    clipKeep,
  ],
  { encoding: "utf8" },
);
if (ff.status !== 0)
  await fail(`ffmpeg render failed (${ff.status}): ${ff.stderr || ff.stdout}`);
copyFileSync(clipKeep, clipDel); 
log(`clips ready: ${clipKeep} + identical copy ${clipDel}`);

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
  ["run", "tauri", "--", "dev", "--no-watch", "--no-dev-server-wait", "-c", tauriConfigPath],
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
await page.screenshot({ path: `${shotDir}/01-cluster.png`, fullPage: false });

await check("selecting the cluster opens the compare view", async () => {
  await page.locator(".card").first().click();
  await page.waitForSelector(".compare", { timeout: 15_000 });
  return true;
});

await check("the compare view lists both duplicate members", async () => {
  await waitFor(
    "two members",
    async () => (await page.locator(".compare .member").count()) >= 2,
    { timeoutMs: 30_000 },
  );
  const n = await page.locator(".compare .member").count();
  return n >= 2 ? true : `expected ≥2 members, saw ${n}`;
});
await page.screenshot({ path: `${shotDir}/02-compare.png`, fullPage: false });

await check("the trash-delete button is enabled with the default (non-best) selection", async () => {
  const btn = page.locator(".compare__summary button");
  await btn.waitFor({ state: "visible", timeout: 10_000 });
  const disabled = await btn.isDisabled();
  return disabled === false ? true : "delete button is disabled for the default selection";
});

await check("confirming the dialog issues the trash delete", async () => {
  await page.locator(".compare__summary button").click();
  await page.waitForSelector(".dlg__content", { timeout: 10_000 });
  const confirmBtn = page.locator(".dlg__actions button.btn--primary, .dlg__actions button.btn--danger");
  await confirmBtn.waitFor({ state: "visible", timeout: 10_000 });
  await confirmBtn.click();
  return true;
});

await check("the trashed copy disappears from the source directory", async () => {
  await waitFor("source file gone", async () => !existsSync(clipDel), {
    timeoutMs: 30_000,
  });
  return !existsSync(clipDel) ? true : `${clipDel} still on disk after trash delete`;
});

await check("the trashed copy lands in the actual Windows Recycle Bin", async () => {
  let found = false;
  await waitFor(
    "recycle bin entry",
    () => {
      found = recycleBinHasPath(clipDel);
      return found;
    },
    { timeoutMs: 60_000, everyMs: 1500 },
  );
  if (found) trashedToPurge.add(clipDel);
  return found ? true : `no Recycle Bin entry with original path ${clipDel}`;
});

await check("the kept (best) copy remains on disk", async () =>
  existsSync(clipKeep) ? true : `${clipKeep} was removed — best copy not preserved`,
);

await check("the now-undersized cluster disappears from the UI", async () => {
  await waitFor(
    "cluster card gone after a user reload",
    async () => {
      await forceUiReload();
      return (await page.locator(".card").count()) === 0;
    },
    { timeoutMs: 60_000, everyMs: 1000 },
  );
  return (await page.locator(".card").count()) === 0
    ? true
    : "a cluster card is still shown after the duplicate was removed";
});
await page.screenshot({ path: `${shotDir}/03-after-delete.png`, fullPage: false });

await check("restoring the file from the Recycle Bin returns it to disk", async () => {
  const restored = restoreFromRecycleBin(clipDel);
  if (!restored) return "Shell restore verb did not run";
  await waitFor("source file restored", async () => existsSync(clipDel), {
    timeoutMs: 30_000,
  });
  trashedToPurge.delete(clipDel); 
  return existsSync(clipDel) ? true : `${clipDel} did not reappear on disk`;
});

await check("the watcher re-indexes the restored file and the cluster re-appears", async () => {
  await waitFor(
    "cluster back after re-index + a user reload",
    async () => {
      await forceUiReload();
      return (await page.locator(".card").count()) > 0;
    },
    { timeoutMs: 180_000, everyMs: 2000 },
  );
  return (await page.locator(".card").count()) > 0
    ? true
    : "the duplicate cluster did not re-appear after restore + re-index";
});
await page.screenshot({ path: `${shotDir}/04-after-restore.png`, fullPage: false });

await check("re-selecting the cluster re-opens the compare view", async () => {
  await page.locator(".card").first().click();
  await page.waitForSelector(".compare", { timeout: 15_000 });
  await waitFor(
    "two members again",
    async () => (await page.locator(".compare .member").count()) >= 2,
    { timeoutMs: 30_000 },
  );
  return true;
});

await check("a permanent-delete erases the copy from disk with no Recycle Bin entry", async () => {
  await page.locator('.compare__modes input[value="permanent"]').check();
  await page.locator(".compare__summary button").click();
  await page.waitForSelector(".dlg__content", { timeout: 10_000 });
  const ack = page.locator(".dlg__ack input[type=checkbox]");
  await ack.waitFor({ state: "visible", timeout: 10_000 });
  await ack.check();
  await page.locator(".dlg__actions button.btn--danger").click();

  await waitFor("perm file gone", async () => !existsSync(clipDel), {
    timeoutMs: 30_000,
  });
  if (existsSync(clipDel)) return `${clipDel} still on disk after permanent delete`;
  await sleep(1500);
  return recycleBinHasPath(clipDel)
    ? "permanent delete left a Recycle Bin entry — it should be a true unlink"
    : true;
});
await page.screenshot({ path: `${shotDir}/05-after-permanent.png`, fullPage: false });

await check("no console / page errors during the live flow", () =>
  consoleErrors.length === 0
    ? true
    : consoleErrors.filter((e) => !/poll failed|will retry|reload failed/i.test(e))
        .join(" | ") || true,
);

if (browser) await browser.close().catch(() => {});
await finish();
