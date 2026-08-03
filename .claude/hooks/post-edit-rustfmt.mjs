import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";

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
  if (!filePath || !filePath.endsWith(".rs")) {
    process.exit(0);
  }

  // rustfmt.toml(edition 2024)이 파일 상위 디렉터리에서 자동 발견되므로 별도 플래그 없이 호출한다.
  spawnSync("rustfmt", [filePath], { encoding: "utf8" });

  // rustfmt가 PATH에 없거나 실패해도 편집 자체를 막지 않는다 — non-blocking.
  process.exit(0);
}

main();
