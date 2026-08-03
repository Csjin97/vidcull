# 알려진 기술 부채 / 성능 개선 후보

2026-08-03에 4개 병렬 에이전트로 수행한 성능 감사 결과. 전부 실제 코드를 읽고 확인한 항목이며, 이 세션에서 이미 적용한 최적화(SIMD popcount 제거, rayon 2단계 병렬화, `prepare_cached` 전환 등)와는 겹치지 않는다. 새 항목을 추가할 때는 파일:라인과 근거를 남기고, 처리하면 상태를 `done`으로 바꾼다.

## High

| 상태 | 위치 | 내용 |
| --- | --- | --- |
| open | `crates/vidcull-parser/src/mp4.rs:166` | `probe_mp4_with_context_cancellable`가 같은 파일을 두 번 연다(크기 확인용 1회 + `read_mp4_tolerant_cancellable`에서 1회). 네이티브 MP4 파싱의 가장 뜨거운 경로. |
| open | `crates/vidcull-matcher/src/partial.rs:323-331` (`AnchorIndex::candidates`) | `BTreeSet<Posting>` 안티패턴 — `near.rs`의 `LshIndex::candidates`에서 이미 고친 것과 동일. `search_inner`의 파일×씬마다 호출되는 hot loop. `Vec::with_capacity + sort_unstable + dedup`로 전환. |
| open | `crates/vidcull-db/src/repo/duplicate_groups.rs` | 파일 전체가 `prepare_cached` 전환에서 빠짐. `add_member`/`find_groups_containing`이 매칭 재구축 시 파일당 1회 호출(`indexing.rs:969,1465`). |
| open | `crates/vidcull-db/src/repo/files.rs:242,265` (`find_active_twin_by_hash`, `list_active_by_hash`) | 같은 파일의 다른 메서드는 이미 캐싱했는데 이 둘만 빠짐. 파일당·스캔당 호출되는 가장 뜨거운 경로 중 하나(`indexing.rs:851,1111,1337`). |
| open | `app/src/routes/(app)/+page.svelte:356-397` + `app/src/lib/model/progress.ts:501-509` (`shouldRefreshGroups`) | 스캔 중 800ms(`ACTIVE_POLL_MS`)마다 사용자가 스크롤로 불러온 전체 클러스터 목록을 재요청하고 `clustersEqual`(O(n·멤버수))로 전체 딥 비교. 목록이 커질수록, 스캔이 길어질수록 부담 증가. |

## Medium

| 상태 | 위치 | 내용 |
| --- | --- | --- |
| open | `crates/vidcull-matcher/src/partial/mih.rs:88-104` (`MultiIndexHash::candidates`) | 위 `AnchorIndex::candidates`와 동일한 `BTreeSet` 패턴의 쌍둥이. |
| open | `crates/vidcull-scanner/src/options.rs`, `walk.rs` | 확장자·제외 디렉터리 판정에서 파일/디렉터리마다 `.to_ascii_lowercase()`로 힙 할당. `eq_ignore_ascii_case` 비교로 무할당 전환 가능. |
| open | `crates/vidcull-parser/src/mp4.rs:331` (`find_box`) | HEVC 파일마다 박스 트리 탐색 시 `Vec`로 전부 수집 후 버림(트랙→미디어→...→엔트리 체이닝). lazy 순회로 전환 가능. |
| open | `crates/vidcull-db/src/repo/similarity_edges.rs`, `scene_hashes.rs`, `partial_mih.rs` | 매칭 재구축당 수백 번 호출되는 insert들이 미캐싱. |
| open | `app/src/routes/(app)/+page.svelte:608-667` (`confirmBulkDelete`) | 일괄 삭제가 클러스터마다 완전 순차 IPC. `Promise.all`로 병렬화 가능(그룹 삭제끼리 상태 공유 없음). |
| open | `crates/vidcull-matcher/src/partial.rs`, `partial/durable.rs` (`votes`/`best` as `BTreeMap`) | 최종 정렬이 이미 있어 `HashMap`으로 바꿔도 결과 동일, B-tree 오버헤드만 제거. |
| open | `app/src/routes/(app)/+page.svelte:575-578` (`allLoadedSelected`) | 체크박스 토글마다 전체 목록 `.every()` 재계산. 선택 개수 카운터로 대체 가능. |

## Low

| 상태 | 위치 | 내용 |
| --- | --- | --- |
| open | `crates/vidcull-core/src/types/path.rs:12` (`NormalizedPath::new`) | 백슬래시 없어도 항상 `.replace()` 할당. `contains('\\')` 선확인이나 `Cow` 반환으로 절감 가능. |
| open | `crates/vidcull-scanner/src/change.rs:41` (`ChangeSet`) | 4개 `Vec`(added/modified/removed/unchanged)에 `reserve` 없이 `push`만. `previous.len()`을 이미 알고 있으니 예약 가능. diff 알고리즘 자체(`BTreeMap::remove` 기반)는 이미 O(n log n)이라 구조적 문제는 없음. |
| open | `crates/vidcull-db/src/repo/delete_journal.rs` | 삭제 배치 루프가 트랜잭션은 이미 감싸져 있지만 statement 캐싱은 안 됨. 사용자 액션당 1회라 빈도는 낮음. |
| open | `crates/vidcull-matcher/src/partial/mih.rs:73-86` (`query_keys`) | 청크당 `Vec<u64>` clone. I/O(SQLite 조회)가 지배적이라 영향 작음. |
