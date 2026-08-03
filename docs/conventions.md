# 코드 작성 관례

## Rust

- 에러는 `thiserror` 기반 타입으로 전파한다. `.unwrap()`/`.expect()`는 테스트 코드이거나 "논리적으로 불가능함이 코드로 증명된" 경우로 한정한다. 데몬 파이프라인·IPC 핸들러 같은 프로덕션 경로에서는 에러를 전파하거나 `catch_unwind` 경계로 격리한다(`docs/invariants.md`).
- DB 접근은 `prepare_cached`로 statement를 캐싱하는 기존 repository 패턴(`crates/vidcull-db/src/repo/`)을 따른다. 매 호출 `.execute()`/`.query_row()`로 새 statement를 매번 준비하지 않는다.
- 대량 데이터를 병렬화할 때는 rayon **2단계 패턴**(병렬 탐색/수집 → 순차 병합)을 우선 검토한다 — union-find처럼 병합 순서가 결과에 영향을 주는 로직을 무분별하게 `par_iter()`로 바꾸지 않는다. 순서 보존이 필요하면 `.par_iter().collect()`나 `par_chunks(...).map(...).collect::<Vec<_>>().into_iter().flatten()` 형태로 순서를 지킨다. 참고 구현: `crates/vidcull-matcher/src/near.rs`, `whole.rs`.
- 후보 집합을 모으는 자리에서는 `BTreeSet` 대신 `Vec::with_capacity(n) + sort_unstable() + dedup()`을 우선 검토한다(같은 결과, 더 적은 할당). 참고: `LshIndex::candidates`(`near.rs`).

## TypeScript / Svelte

- Svelte 5 runes(`$state`, `$derived`, `$props`, `$effect`) 관례를 따르고, 클래스 기반 스토어나 구식 `writable()` 패턴을 새로 도입하지 않는다.
- IPC 응답에는 `as` 타입 단언을 쓰지 않고 `app/src/lib/ipc/validate.ts`의 런타임 검증을 거친다.
- 큰 목록(클러스터·파일 목록)은 `VirtualList.svelte`로 윈도잉한다. 잠재적으로 큰 배열에 무제한 `{#each}`를 새로 추가하지 않는다.

## 플랫폼 관례

- Windows 전용 동작(`explorer.exe /select,` 등)은 인용/이스케이프 방식이 표준 argv 규칙과 다를 수 있다 — 공백·특수문자가 포함된 경로로 반드시 테스트한다.

## 변경 이력

이 프로젝트는 git으로 관리된다. 파일 상단에 changelog 주석 블록을 새로 추가하지 않는다 — 커밋 메시지와 `git log`/`git blame`이 변경 이력의 단일 소스다. 커밋 메시지는 무엇을 바꿨는지보다 **왜** 바꿨는지를 담는다.
