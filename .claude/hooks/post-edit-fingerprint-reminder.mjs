import { readFileSync } from "node:fs";

function readStdinJson() {
  return JSON.parse(readFileSync(0, "utf8"));
}

function main() {
  let input;
  try {
    input = readStdinJson();
  } catch {
    process.exit(0);
  }

  const filePath = input?.tool_input?.file_path;
  if (!filePath) {
    process.exit(0);
  }

  const normalized = filePath.replace(/\\/g, "/");
  const touchesFingerprint =
    normalized.includes("crates/vidcull-fingerprint/src/") ||
    normalized.includes("crates/vidcull-fingerprint/tests/");

  if (!touchesFingerprint) {
    process.exit(0);
  }

  process.stdout.write(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "PostToolUse",
        additionalContext:
          "지문 계산 경로(vidcull-fingerprint)를 편집했다 — docs/invariants.md의 결정성 규칙에 따라 " +
          "`cargo test -p vidcull-fingerprint`로 golden bit 테스트(golden_*_to_bits, simd_*_is_bit_identical_to_scalar_reference 등)가 " +
          "그대로 통과하는지 반드시 재검증한다.",
      },
    }),
  );
  process.exit(0);
}

main();
