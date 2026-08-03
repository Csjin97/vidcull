
import { execFileSync, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { classifyFreshness } from "../src/lib/build/freshness.js";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const appRoot = dirname(scriptDir);

function resolveRepoRoot() {
  try {
    return execFileSync("git", ["rev-parse", "--show-toplevel"], {
      encoding: "utf8",
      cwd: appRoot,
    }).trim();
  } catch {
    return null;
  }
}

function resolveHostTriple() {
  const output = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
  const line = output.split(/\r?\n/).find((l) => l.startsWith("host:"));
  if (!line) {
    throw new Error("could not find `host:` line in `rustc -vV` output");
  }
  return line.slice("host:".length).trim();
}

function resolveHeadSha() {
  try {
    return execFileSync("git", ["rev-parse", "--short", "HEAD"], {
      encoding: "utf8",
      cwd: appRoot,
    }).trim();
  } catch {
    return "";
  }
}

function main() {
  const repoRoot = resolveRepoRoot();
  if (!repoRoot) {
    console.warn(
      "[verify-build-freshness] SKIP: not a git repository, cannot verify daemon freshness against HEAD",
    );
    process.exit(0);
  }

  const triple = resolveHostTriple();
  const isWindows = triple.includes("windows");
  const exeName = isWindows
    ? `vidcull-daemon-${triple}.exe`
    : `vidcull-daemon-${triple}`;
  const exePath = join(repoRoot, "app", "src-tauri", "binaries", exeName);
  const exeExists = existsSync(exePath);

  const headSha = resolveHeadSha();

  let embeddedStamp = "";
  if (exeExists) {
    const result = spawnSync(exePath, ["--build-stamp"], {
      encoding: "utf8",
      timeout: 20_000,
    });
    if (result.error || result.status !== 0) {
      console.error(
        `[verify-build-freshness] EXEC_FAIL: failed to run ${exePath} --build-stamp`,
      );
      if (result.error) {
        console.error(`[verify-build-freshness]   error: ${result.error.message}`);
      }
      if (typeof result.status === "number" && result.status !== 0) {
        console.error(`[verify-build-freshness]   exit code: ${result.status}`);
      }
      if (result.stderr) {
        console.error(`[verify-build-freshness]   stderr: ${result.stderr.trim()}`);
      }
      process.exit(1);
    }
    embeddedStamp = result.stdout.trim();
  }

  const outcome = classifyFreshness({ embeddedStamp, headSha, exeExists });

  if (!outcome.fresh) {
    console.error(`[verify-build-freshness] ${outcome.code}: ${outcome.reason}`);
    process.exit(1);
  }

  if (outcome.dirty) {
    console.warn(
      "[verify-build-freshness] WARN: staged daemon matches HEAD but was built from a dirty working tree",
    );
  } else {
    console.log("[verify-build-freshness] OK: staged daemon matches HEAD");
  }
  process.exit(0);
}

main();
