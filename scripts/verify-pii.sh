#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

tokens_b64=(
  "YXZbXy1dP3NvcnQ="   # scrubbed dataset name (regex; word-anchored below)
  "MXRhbmcyYmFuZzky"   # scrubbed team slug
  "dW5ndXNvMTIzNA=="   # scrubbed user handle
)

exclude_paths=(
  ":(exclude)scripts/verify-pii.sh"
  ":(exclude).githooks/pre-push"
  ":(exclude)**/package-lock.json"
  ":(exclude)**/*.lock"
  ":(exclude)Cargo.lock"
)

status=0
for b64 in "${tokens_b64[@]}"; do
  pat="$(printf '%s' "$b64" | base64 --decode)"
  hits="$(git grep -IEni "\\b${pat}\\b" -- "${exclude_paths[@]}" || true)"
  hits="$(printf '%s' "$hits" | grep -v 'pii-ok' || true)"
  if [ -n "$hits" ]; then
    echo "::error::PII regression — scrubbed identifier reintroduced in tracked files:"
    printf '%s\n' "$hits"
    status=1
  fi
done

if [ "$status" -ne 0 ]; then
  echo "PII guard: FAIL — remove the identifiers above, or mark an intentional line with 'pii-ok'."
  exit 1
fi
echo "PII guard: clean"
