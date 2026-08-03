---
description: vidcull-fingerprint의 golden bit 결정성 테스트를 실행하고 pass/fail을 보고한다
allowed-tools: Bash
---

`docs/invariants.md`의 결정성 규칙을 검증한다. 지문 계산 경로(`crates/vidcull-fingerprint`)를 건드린 직후 실행한다.

1. `cargo test -p vidcull-fingerprint`를 실행한다.
2. 출력에서 `golden_*_to_bits`, `simd_*_is_bit_identical_to_scalar_reference` 계열 테스트 이름을 찾아 하나하나 pass/fail 여부를 명시한다 — "전체 통과"로 뭉뚱그리지 않는다.
3. 하나라도 실패하면 어떤 테스트가 실패했는지, 직전에 어떤 파일을 편집했는지(`git diff --stat`)를 함께 보고한다 — golden 값 자체를 실패에 맞춰 바꾸는 것은 금지되어 있다(`docs/invariants.md`).
4. 전부 통과하면 통과한 golden 테스트 이름 목록과 함께 짧게 보고한다.
