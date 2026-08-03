# 불변식 — 절대 깨면 안 되는 것

vidcull 코드를 고치기 전에 확인한다. 여기 있는 규칙은 "더 낫다/더 정확하다"는 이유로도 예외를 두지 않는다. 왜 이런 규칙이 있는지는 `ARCHITECTURE.md`의 "설계 불변식" 절에 배경이 있다.

## 결정성

`crates/vidcull-fingerprint`의 지문 계산 경로(Tier 1 전역 + Tier 2 시간축)는 핀된 ffmpeg(vendored, SHA-256 검증)와 정준 디코드 그리드 위에서 시드 동일 → 바이트 동일을 보장한다. `golden_*_to_bits`, `simd_*_is_bit_identical_to_scalar_reference` 같은 golden bit 테스트는 반드시 그대로 통과해야 한다.

- "더 정확해 보이는" 계산 방식 변경이라도 골든 값 자체를 바꾸지 않는다.
- 값이 조금이라도 바뀌면 이미 저장된 지문과의 시드 동일→바이트 동일 재현성이 깨진다.

## panic 격리

`vidcull-daemon`은 파일별 디코드 panic이 데몬 전체를 죽이지 않도록 `catch_unwind`에 의존한다(`crates/vidcull-daemon/src/indexing.rs`, 테스트 `panic_in_decode_becomes_decode_error_not_unwind`).

- `panic = "abort"`를 어떤 프로필에도 추가하지 않는다.
- 이 catch_unwind 경계를 우회하거나 제거하지 않는다.

## 삭제 안전

실제 파일 삭제는 반드시 **OS 휴지통 경유 + DB soft-delete + 저널/undo**를 거친다.

- 직접 파일시스템 `remove_file` 등으로 대체하지 않는다.
- 삭제 관련 코드를 고칠 때는 실수 삭제 후 복원 감지 경로도 함께 확인한다.

## IPC 버전 게이트

`vidcull-ipc`가 소유한 프로토콜 버전(현재 v11) 불일치는 하드 게이트로 거부되어야 한다.

- 버전 체크를 완화하거나, 특정 케이스에 한해 우회하는 코드를 추가하지 않는다.
- 프론트 TS는 IPC 응답을 `app/src/lib/ipc/validate.ts`에서 런타임 검증한다 — `as` 단언으로 이 경로를 건너뛰지 않는다.

## 메타 예산

영상 1개당 DB 메타데이터는 **20KB 이하**를 유지한다. 새 컬럼/필드를 추가할 때 이 예산 안에 들어오는지 먼저 계산한다.

## unsafe_code 기조

크레이트 대부분이 `unsafe_code = "forbid"`(또는 `deny`) 기조다.

- 새 unsafe 블록을 임의로 추가하지 않는다.
- 불가피한 OS FFI(예: `crates/vidcull-daemon/src/metrics.rs`)만 국소 `#[allow(unsafe_code)]` + `// SAFETY: ...` 주석으로 격리한다.

## 네이티브 디코더 수용 범위 (재논의 금지)

AV1·VP9·10-bit·tiles·transquant-bypass·PCM은 네이티브 디코더 대상이 **아니며** 항상 ffmpeg 폴백으로 처리한다. 네이티브 게이트는 "픽셀 bit-exact"가 아니라 `phash(네이티브 프레임) == phash(ffmpeg 골든 프레임)`(지문 동일성)이다 — 픽셀 단위 미세 차이(predictor 라운딩 등)는 wontfix.
