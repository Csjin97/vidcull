---
description: docs/invariants.md·commands.md·tech-debt.md가 언급하는 파일 경로/심볼이 실제 코드와 맞는지 스팟체크한다
allowed-tools: Read, Grep, Bash
---

`docs/` 아래 참조 문서가 실제 코드와 어긋나지 않았는지(코드가 바뀌었는데 문서가 안 바뀐 경우) 사람이 검토할 수 있게 점검한다. 자동으로 고치거나 PR을 만들지 않는다 — 결과만 보고한다.

1. `docs/invariants.md`, `docs/commands.md`, `docs/tech-debt.md`를 읽고, 각 문서가 인용하는 파일 경로와 함수/테스트 이름을 추출한다(예: `crates/vidcull-daemon/src/indexing.rs`의 `panic_in_decode_becomes_decode_error_not_unwind`, `docs/tech-debt.md`의 각 항목이 가리키는 파일:라인 등).
2. 각 항목에 대해 파일이 실제로 존재하는지, 인용된 심볼/테스트 이름이 grep으로 실제로 발견되는지 확인한다.
3. `docs/tech-debt.md`의 각 항목은 인용된 라인 번호 근처에 해당 코드가 여전히 그 모습으로 있는지도 간단히 확인한다(이미 고쳐졌으면 `done`으로 표시할 후보로 보고).
4. 다음 형식으로 보고한다: 정상 확인된 항목 수, 파일/심볼이 사라졌거나 라인이 크게 어긋난 stale 항목 목록(문서 위치 + 이유), `docs/tech-debt.md`에서 이미 해결된 것으로 보이는 항목 목록. 발견한 문제를 직접 고치지 말고 사용자에게 보고한다.
