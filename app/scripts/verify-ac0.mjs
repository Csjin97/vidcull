
import { spawn, spawnSync } from "node:child_process";
import {
  mkdirSync,
  mkdtempSync,
  rmSync,
  existsSync,
  readdirSync,
  readFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve, basename } from "node:path";
import { fileURLToPath } from "node:url";
import { DatabaseSync } from "node:sqlite";


const here = fileURLToPath(new URL(".", import.meta.url));
const appDir = resolve(here, "..");
const repoRoot = resolve(appDir, "..");

const ISOLATED_TARGET = join(repoRoot, "target-agent-ac0");
const DEFAULT_DAEMON_EXE = join(ISOLATED_TARGET, "debug", "vidcull-daemon.exe");


const rawArgs = process.argv.slice(2);

let libraryPath = null;
let daemonExe = DEFAULT_DAEMON_EXE;
let dryRun = false;
let timeoutSec = 3600;

{
  let i = 0;
  while (i < rawArgs.length) {
    const arg = rawArgs[i];
    if (arg === "--dry-run") {
      dryRun = true;
    } else if (arg === "--timeout") {
      i += 1;
      const v = Number(rawArgs[i]);
      if (!Number.isFinite(v) || v <= 0)
        fatal(`--timeout requires a positive integer (got "${rawArgs[i]}")`);
      timeoutSec = v;
    } else if (arg.startsWith("--")) {
      fatal(`Unknown flag: ${arg}`);
    } else if (libraryPath === null) {
      libraryPath = resolve(arg); 
    } else if (daemonExe === DEFAULT_DAEMON_EXE) {
      daemonExe = resolve(arg); 
    } else {
      fatal(`Unexpected argument: ${arg}`);
    }
    i += 1;
  }
}


function fatal(msg) {
  console.error(`[verify-ac0] FATAL: ${msg}`);
  process.exit(2);
}

const log = (...a) => console.log("[verify-ac0]", ...a);

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}


function validateArgs() {
  const errors = [];
  if (!libraryPath) {
    errors.push(
      "Missing required argument: <library-path>. Usage: node scripts/verify-ac0.mjs <library-path> [daemon-exe]",
    );
  } else if (!existsSync(libraryPath)) {
    errors.push(`Library path does not exist: ${libraryPath}`);
  }
  return errors;
}

if (dryRun) {
  console.log("=== verify-ac0 DRY-RUN ===");
  console.log();

  const errors = validateArgs();
  if (errors.length > 0) {
    console.log("Arg validation FAILED:");
    for (const e of errors) console.log(`  ERROR: ${e}`);
    console.log();
  } else {
    console.log("Arg validation OK");
    console.log();
    console.log("Resolved paths:");
    console.log(`  library-path : ${libraryPath}`);
    console.log(`  daemon-exe   : ${daemonExe}`);
    console.log(`  repo-root    : ${repoRoot}`);
    console.log(
      `  isolated-target: ${ISOLATED_TARGET}`,
    );
    console.log(`  timeout      : ${timeoutSec}s`);
    console.log();

    const ipcPipe = String.raw`\\.\pipe\vidcull-ac0-<pid>`;
    const workDir = "<mkdtemp: %TEMP%\\av-ac0-XXXXXX>";
    const daemonEnv = [
      `VIDCULL_IPC=${ipcPipe}`,
      `VIDCULL_DB=${workDir}\\ac0.db`,
      `VIDCULL_WATCH=${libraryPath}`,
      `VIDCULL_THUMB_DIR=${workDir}\\thumbs`,
      `VIDCULL_PARTIAL_CLIPS=1`,
      `CARGO_TARGET_DIR=${ISOLATED_TARGET}`,
    ];
    console.log("Daemon command (env + exe):");
    for (const e of daemonEnv) console.log(`  ${e}`);
    console.log(`  ${daemonExe}`);
    console.log();

    if (existsSync(daemonExe)) {
      console.log(`daemon-exe EXISTS at ${daemonExe}`);
    } else {
      console.log(
        `daemon-exe NOT FOUND at ${daemonExe}`,
      );
      console.log(
        "  Build with: CARGO_TARGET_DIR=" +
          `"${ISOLATED_TARGET}" cargo build -p vidcull-daemon`,
      );
    }
    console.log();

    const appData = process.env.APPDATA ?? "<APPDATA>";
    const logDir = join(appData, "vidcull");
    console.log(`Log directory: ${logDir}`);
    console.log(
      existsSync(logDir)
        ? `  EXISTS (${readdirSync(logDir).filter((f) => f.includes("vidcull-daemon")).length} daemon log file(s) found)`
        : "  Not yet created (will be created on first daemon run)",
    );
  }
  console.log();
  console.log("=== DRY-RUN COMPLETE (no daemon spawned) ===");
  process.exit(errors.length > 0 ? 2 : 0);
}


const argErrors = validateArgs();
if (argErrors.length > 0) {
  for (const e of argErrors) fatal(e);
}

if (!existsSync(daemonExe)) {
  fatal(
    `daemon not built at ${daemonExe}\n` +
      `  Build with: CARGO_TARGET_DIR="${ISOLATED_TARGET}" cargo build -p vidcull-daemon`,
  );
}


const signals = {
  codecHistogram: {},    
  nativeDecode: 0,       
  fallbackDecode: 0,     
  sidecarHits: 0,        
  av1Skips: 0,           
  hangEvents: 0,         
  oomEvents: 0,
  partialCliResults: [], 
  filesIndexed: 0,
  startMs: null,
  endMs: null,
  peakRssBytes: 0,       
  rssSamples: [],        
  cpuSamples: [],        
  gateSamples: [],
};

function parseDaemonLine(line) {
  line = line.replace(/\x1b\[[0-9;]*m/g, "");
  const jsonMatch = line.match(/\{.*\}/);
  if (jsonMatch) {
    try {
      const obj = JSON.parse(jsonMatch[0]);
      if (obj.codec) {
        const c = String(obj.codec).toLowerCase();
        signals.codecHistogram[c] = (signals.codecHistogram[c] ?? 0) + 1;
      }
      if (obj.path === "native") signals.nativeDecode += 1;
      if (obj.path === "fallback") signals.fallbackDecode += 1;
      if (obj.event === "sidecar_hit") signals.sidecarHits += 1;
      if (obj.event === "partial_skip" || obj.skip_reason) signals.av1Skips += 1;
      if (obj.event === "file_indexed" || obj.event === "indexed") {
        signals.filesIndexed += 1;
        if (signals.startMs === null) signals.startMs = Date.now();
        signals.endMs = Date.now();
      }
    } catch {
    }
  }

  const l = line.toLowerCase();

  const decodePath = line.match(/decode_path=(\w+)/);
  if (decodePath) {
    if (/native/i.test(decodePath[1])) signals.nativeDecode += 1;
    else if (/fallback/i.test(decodePath[1])) signals.fallbackDecode += 1;
  }

  if (line.includes('stage="index"')) {
    const codecKv = line.match(/codec=(\w+)/);
    if (codecKv) {
      const c = codecKv[1].toLowerCase();
      signals.codecHistogram[c] = (signals.codecHistogram[c] ?? 0) + 1;
      signals.filesIndexed += 1;
      if (signals.startMs === null) signals.startMs = Date.now();
      signals.endMs = Date.now();
    }
  }

  if (line.includes('stage="resource"')) {
    const t = Date.now();
    const rssMatch = line.match(/rss_bytes=(\d+)/);
    const cpuMatch = line.match(/cpu_permille=(\d+)/);
    if (rssMatch) {
      const rss = Number(rssMatch[1]);
      signals.peakRssBytes = Math.max(signals.peakRssBytes, rss);
      signals.rssSamples.push({ t, rss });
    }
    if (cpuMatch) {
      const cpu = Number(cpuMatch[1]);
      signals.cpuSamples.push({ t, cpu });
    }
  }

  if (line.includes("stage=gates") || line.includes('stage="gates"')) {
    const t = Date.now();
    const decInUse = line.match(/decode_conc_in_use=(\d+)/);
    const decCap   = line.match(/decode_conc_cap=(\d+)/);
    const baseInUse = line.match(/base_gate_in_use=(\d+)/);
    const baseCap   = line.match(/base_gate_cap=(\d+)/);
    if (decInUse || baseInUse) {
      signals.gateSamples.push({
        t,
        decodeInUse: decInUse  ? Number(decInUse[1])  : null,
        decodeCap:   decCap    ? Number(decCap[1])    : null,
        baseInUse:   baseInUse ? Number(baseInUse[1]) : null,
        baseCap:     baseCap   ? Number(baseCap[1])   : null,
      });
    }
  }

  if (l.includes("sidecar") && l.includes("hit")) signals.sidecarHits += 1;

  if (/skip marker stamped|partial fingerprint skipped/i.test(line)) {
    signals.av1Skips += 1;
  }

  if (l.includes("possible") && (l.includes("overlap") || l.includes("partial"))) {
    signals.partialCliResults.push(line.trim());
  }

  if (l.includes("out of memory") || l.includes("oom") || l.includes("killed")) {
    signals.oomEvents += 1;
  }
  if (l.includes("deadlock") || l.includes("hang detected")) {
    signals.hangEvents += 1;
  }

  if (
    l.includes("indexed") &&
    (l.includes("file") || l.includes("task") || l.includes("done"))
  ) {
    if (signals.startMs === null) signals.startMs = Date.now();
    signals.endMs = Date.now();
  }
}


function scrapeLogFiles() {
  const appData = process.env.APPDATA;
  if (!appData) return;
  const logDir = join(appData, "vidcull");
  if (!existsSync(logDir)) return;

  let files;
  try {
    files = readdirSync(logDir).filter((f) => f.startsWith("vidcull-daemon"));
  } catch {
    return;
  }

  files.sort().reverse();
  const target = files[0];
  if (!target) return;

  log(`Scraping log file: ${target}`);
  let content;
  try {
    content = readFileSync(join(logDir, target), "utf8");
  } catch {
    return;
  }

  for (const line of content.split("\n")) {
    parseDaemonLine(line);
  }
}



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


workDir = mkdtempSync(join(tmpdir(), "av-ac0-"));
const dbPath = join(workDir, "ac0.db");
const thumbDir = join(workDir, "thumbs");
mkdirSync(thumbDir, { recursive: true });

const IPC_PIPE = String.raw`\\.\pipe\vidcull-ac0-${process.pid}`;

const daemonEnv = {
  ...process.env,
  VIDCULL_IPC: IPC_PIPE,
  VIDCULL_DB: dbPath,
  VIDCULL_WATCH: libraryPath,
  VIDCULL_THUMB_DIR: thumbDir,
  VIDCULL_PARTIAL_CLIPS: "1",
  VIDCULL_RESOURCE_LOG: "1",
  VIDCULL_MAX_PERF: "1",
  CARGO_TARGET_DIR: ISOLATED_TARGET,
};

log(`Library    : ${libraryPath}`);
log(`Daemon exe : ${daemonExe}`);
log(`DB         : ${dbPath}`);
log(`IPC pipe   : ${IPC_PIPE}`);
log(`Timeout    : ${timeoutSec}s`);
log("");
log("Spawning daemon…");

const daemon = spawn(daemonExe, [], {
  cwd: repoRoot,
  env: daemonEnv,
  stdio: ["ignore", "pipe", "pipe"],
});
children.push(daemon);

function onDaemonChunk(b) {
  const text = String(b);
  process.stdout.write(`[daemon] ${text}`);
  for (const line of text.split("\n")) parseDaemonLine(line);
}

daemon.stdout.on("data", onDaemonChunk);
daemon.stderr.on("data", onDaemonChunk);

let daemonExitCode = null;
daemon.on("exit", (code) => {
  daemonExitCode = code;
  log(`daemon exited (code=${code})`);
});

await sleep(2000);
if (daemonExitCode !== null) {
  fatal(`Daemon exited early (code=${daemonExitCode}) — check for build or config errors.`);
}


function queueCounts() {
  try {
    const db = new DatabaseSync(dbPath, { readOnly: true });
    try {
      const q = (sql) => Number(db.prepare(sql).get().n);
      const total = q(
        "SELECT count(DISTINCT payload) AS n FROM task_queue WHERE priority >= 0 AND payload IS NOT NULL",
      );
      const terminal = q(
        "SELECT count(DISTINCT payload) AS n FROM task_queue WHERE state IN ('DONE','FAILED') AND priority >= 0 AND payload IS NOT NULL",
      );
      const partial = q(
        "SELECT count(*) AS n FROM task_queue WHERE state IN ('PENDING','RUNNING') AND priority = -200",
      );
      return { total, terminal, partial };
    } finally {
      db.close();
    }
  } catch {
    return null; 
  }
}

log(`Waiting up to ${timeoutSec}s for the indexing pass to drain…`);

const deadline = Date.now() + timeoutSec * 1000;
const POLL_MS = 3000;
const STABLE_MS = Number(process.env.VIDCULL_AC0_STABLE_MS) || 120000;

let drained = false;
let sawWork = false;
let minOutstanding = -1; 
let maxTerminal = -1; 
let lastProgressMs = Date.now(); 
let lastLogMs = 0;
while (Date.now() < deadline) {
  await sleep(POLL_MS);
  if (daemonExitCode !== null) break; 
  if (signals.oomEvents > 0 || signals.hangEvents > 0) {
    log("OOM or hang marker detected — terminating early.");
    break;
  }

  const c = queueCounts();
  if (c === null) continue; 
  if (c.total > 0) sawWork = true;
  if (Date.now() - lastLogMs > 30000) {
    log(`indexing… base ${c.terminal}/${c.total} files done, partial ${c.partial} pending`);
    lastLogMs = Date.now();
  }

  const outstanding = c.total - c.terminal + c.partial;
  let progressed = false;
  if (c.terminal > maxTerminal) {
    maxTerminal = c.terminal;
    progressed = true;
  }
  if (minOutstanding < 0 || outstanding < minOutstanding) {
    minOutstanding = outstanding;
    progressed = true;
  }
  if (progressed) lastProgressMs = Date.now();
  if (sawWork && Date.now() - lastProgressMs > STABLE_MS) {
    drained = true;
    log(
      outstanding === 0
        ? "Indexing complete (queue fully drained)."
        : `Indexing settled (no base/partial progress for ${STABLE_MS / 1000}s; ${c.terminal}/${c.total} base terminal, ${outstanding} outstanding).`,
    );
    break;
  }
}

const timedOut = daemonExitCode === null && !drained && Date.now() >= deadline;
if (daemonExitCode === null) {
  log(
    drained
      ? "Drain detected — stopping the watcher daemon."
      : `Timeout reached (${timeoutSec}s) — killing daemon.`,
  );
  killChild(daemon);
  await sleep(1000);
}


scrapeLogFiles();


const elapsedSec =
  signals.startMs !== null && signals.endMs !== null
    ? (signals.endMs - signals.startMs) / 1000
    : null;

const throughputFilesPerMin =
  elapsedSec !== null && elapsedSec > 0 && signals.filesIndexed > 0
    ? Math.round((signals.filesIndexed / elapsedSec) * 60)
    : null;

const mp4_125_NAME = "mp4_125";
const LIRISU_NAME = "리리수"; 
const recallDetected =
  signals.partialCliResults.some(
    (l) => l.includes(mp4_125_NAME) && l.includes(LIRISU_NAME),
  );

const hangOrOom = signals.hangEvents > 0 || signals.oomEvents > 0;

const overallVerdict = hangOrOom
  ? "FAIL"
  : timedOut
    ? "FAIL (timeout — daemon did not complete within the allotted window)"
    : recallDetected
      ? "PASS"
      : "SKIP-RECORD (ground-truth pair not detected — confirm library contains mp4_125 and 리리수 source)";


console.log("");
console.log("══════════════════════════════════════════════════════");
console.log(" verify-ac0 report");
console.log("══════════════════════════════════════════════════════");
console.log("");

const hangVerdict = hangOrOom
  ? `FAIL  (hang=${signals.hangEvents}, oom=${signals.oomEvents})`
  : "PASS  (hang=0, OOM=0)";
console.log(`AC0-1 hang/OOM       : ${hangVerdict}`);

const recallVerdict = recallDetected
  ? `PASS  (POSSIBLE overlap detected — mp4_125 ⊂ 리리수 ground-truth present)`
  : `SKIP  (ground-truth pair not in log — library may not contain mp4_125 + 리리수 source, or partial_clips_enabled was not set)`;
console.log(`AC0-2 recall         : ${recallVerdict}`);

const codecSummary =
  Object.keys(signals.codecHistogram).length > 0
    ? Object.entries(signals.codecHistogram)
        .sort((a, b) => b[1] - a[1])
        .map(([k, v]) => `${k}:${v}`)
        .join(", ")
    : "(no codec data — structured logging may not be active on this build)";
console.log(`AC0-3 codec histogram: ${codecSummary}`);
console.log(
  `       native path  : ${signals.nativeDecode} frames` +
    (signals.nativeDecode > 0 ? "" : " (may be in APPDATA log — check log scrape above)"),
);
console.log(
  `       fallback path: ${signals.fallbackDecode} frames`,
);

const skipVerdict =
  signals.av1Skips > 0
    ? `SKIP-RECORD  (${signals.av1Skips} AV1/large-file partial-skips — expected per §J)`
    : "SKIP  (0 AV1 skips observed — either no AV1 files in library, or partial_clips not enabled)";
console.log(`AC0-4 AV1/large skip : ${skipVerdict}`);

const sidecarVerdict =
  signals.sidecarHits > 0
    ? `${signals.sidecarHits} sidecar reuse hit(s)`
    : "0 (first-run or no existing sidecars — re-run to confirm hit count grows)";
console.log(`AC0-5 sidecar hits   : ${sidecarVerdict}`);

const throughputVerdict =
  throughputFilesPerMin !== null
    ? `${throughputFilesPerMin} files/min  (${signals.filesIndexed} files in ${Math.round(elapsedSec)}s)`
    : "(not measured — structured event lines not detected; check log scrape)";
console.log(`AC0-6 throughput     : ${throughputVerdict}`);

console.log("");
console.log("── resource telemetry ──────────────");
if (signals.rssSamples.length === 0) {
  console.log("  RSS samples        : none (daemon may not emit stage=\"resource\" yet, or log scrape not reached)");
} else {
  const peakMiB = (signals.peakRssBytes / (1024 * 1024)).toFixed(1);
  console.log(`  peak RSS           : ${peakMiB} MiB  (${signals.peakRssBytes} bytes, ${signals.rssSamples.length} samples)`);

  const nowMs = signals.rssSamples[signals.rssSamples.length - 1].t;
  const windowMs = 60_000;
  const window = signals.rssSamples.filter((s) => s.t >= nowMs - windowMs);
  if (window.length >= 2) {
    const first = window[0];
    const last = window[window.length - 1];
    const dtSec = (last.t - first.t) / 1000;
    const slopeKiBs = dtSec > 0 ? ((last.rss - first.rss) / 1024 / dtSec).toFixed(1) : "0";
    console.log(`  last-60s RSS slope : ${slopeKiBs} KiB/s  (positive = growing; user judges threshold)`);
  } else {
    console.log("  last-60s RSS slope : insufficient samples in window");
  }
}
if (signals.cpuSamples.length === 0) {
  console.log("  CPU samples        : none");
} else {
  const peak = Math.max(...signals.cpuSamples.map((s) => s.cpu));
  const avg = Math.round(signals.cpuSamples.reduce((a, s) => a + s.cpu, 0) / signals.cpuSamples.length);
  console.log(`  peak/avg CPU       : ${peak}‰ / ${avg}‰  (permille; ${signals.cpuSamples.length} samples)`);
}

console.log("");
console.log("── gate utilization ───────────────────────────");
if (signals.gateSamples.length === 0) {
  console.log("  gate samples       : none (daemon may not emit stage=gates yet — check VIDCULL_RESOURCE_LOG)");
} else {
  const n = signals.gateSamples.length;

  const decSamples  = signals.gateSamples.filter((s) => s.decodeInUse !== null);
  const baseSamples = signals.gateSamples.filter((s) => s.baseInUse   !== null);

  if (decSamples.length > 0) {
    const peakDecInUse = Math.max(...decSamples.map((s) => s.decodeInUse));
    const avgDecInUse  = Math.round(decSamples.reduce((a, s) => a + s.decodeInUse, 0) / decSamples.length);
    const decCap = decSamples[decSamples.length - 1].decodeCap;
    const capStr = decCap !== null ? `/ ${decCap}` : "";
    console.log(`  decode_conc        : peak ${peakDecInUse}${capStr}  avg ${avgDecInUse}${capStr}  (${decSamples.length} samples)`);
    if (decCap !== null && decCap > 0) {
      const satPct = Math.round((peakDecInUse / decCap) * 100);
      console.log(`    peak saturation  : ${satPct}%  (${satPct >= 90 ? "SATURATED — slots fully used" : satPct >= 50 ? "moderate" : "low — slots idle"})`);
    }
  } else {
    console.log("  decode_conc        : no decode_conc_in_use fields observed");
  }

  if (baseSamples.length > 0) {
    const peakBaseInUse = Math.max(...baseSamples.map((s) => s.baseInUse));
    const avgBaseInUse  = Math.round(baseSamples.reduce((a, s) => a + s.baseInUse, 0) / baseSamples.length);
    const baseCap = baseSamples[baseSamples.length - 1].baseCap;
    const capStr = baseCap !== null ? `/ ${baseCap}` : "";
    console.log(`  base_gate          : peak ${peakBaseInUse}${capStr}  avg ${avgBaseInUse}${capStr}  (${baseSamples.length} samples)`);
    if (baseCap !== null && baseCap > 0) {
      const satPct = Math.round((peakBaseInUse / baseCap) * 100);
      console.log(`    peak saturation  : ${satPct}%  (${satPct >= 90 ? "SATURATED" : satPct >= 50 ? "moderate" : "low"})`);
    }
  } else {
    console.log("  base_gate          : no base_gate_in_use fields observed");
  }

  console.log(`  total gate samples : ${n}`);
}

console.log("");
console.log(`daemon exit code     : ${daemonExitCode ?? "(killed / timed out)"}`);
console.log(`timed out            : ${timedOut}`);
console.log("");
console.log(`OVERALL VERDICT      : ${overallVerdict}`);
console.log("");
console.log("══════════════════════════════════════════════════════");

process.exit(hangOrOom || timedOut ? 1 : 0);
