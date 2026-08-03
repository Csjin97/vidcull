import { readFileSync } from "node:fs";

const DANGEROUS_PATTERNS = [
  { re: /\bgit\s+push\b[^\n]*(--force(-with-lease)?\b|(?:^|\s)-f\b)/, why: "강제 push" },
  { re: /\bgit\s+reset\b[^\n]*--hard\b/, why: "git reset --hard" },
  { re: /\bgit\s+clean\b[^\n]*-f/, why: "git clean -f" },
];

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

  if (input?.tool_name !== "Bash") {
    process.exit(0);
  }

  const command = input?.tool_input?.command;
  if (!command) {
    process.exit(0);
  }

  const hit = DANGEROUS_PATTERNS.find((p) => p.re.test(command));
  if (!hit) {
    process.exit(0);
  }

  process.stdout.write(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "deny",
        permissionDecisionReason:
          `파괴적 git 작업(${hit.why})은 이 저장소에서 hook으로 차단된다. ` +
          "정말 필요하면 사용자에게 직접 실행을 요청하거나 .claude/settings.json의 PreToolUse hook을 일시적으로 조정한다.",
      },
    }),
  );
  process.exit(0);
}

main();
