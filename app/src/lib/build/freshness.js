

export function classifyFreshness({ embeddedStamp, headSha, exeExists }) {
  if (exeExists === false) {
    return {
      fresh: false,
      code: "MISSING",
      dirty: false,
      reason:
        "daemon not staged at app/src-tauri/binaries/ — run stage-daemon.ps1",
    };
  }

  if (!headSha) {
    return {
      fresh: false,
      code: "NO_GIT",
      dirty: false,
      reason: "cannot resolve git HEAD; freshness unverifiable",
    };
  }

  const stampToken = (embeddedStamp ?? "").trim().split(/\s+/)[0] ?? "";

  if (stampToken === "unknown") {
    return {
      fresh: false,
      code: "UNKNOWN",
      dirty: false,
      reason:
        'daemon stamp is "unknown" (git absent at daemon build); cannot verify',
    };
  }

  const dirty = stampToken.endsWith("-dirty");
  const baseSha = dirty ? stampToken.slice(0, -"-dirty".length) : stampToken;

  if (baseSha === headSha) {
    return { fresh: true, code: "FRESH", dirty };
  }

  return {
    fresh: false,
    code: "STALE",
    dirty,
    reason:
      `bundled daemon ${baseSha} != HEAD ${headSha}; re-run app/scripts/stage-daemon.ps1 (or npm run build:installer). ` +
      "NOTE: guard checks HEAD-commit identity, not uncommitted edits — commit + re-stage.",
  };
}
