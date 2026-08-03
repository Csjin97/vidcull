# 빌드·테스트·검증 명령

| 목적 | 명령 |
| --- | --- |
| Rust 정적 확인 | `cargo check --workspace` |
| Rust 전체 테스트 | `cargo test --workspace` |
| 특정 크레이트만 테스트 | `cargo test -p <crate-name>` (예: `vidcull-fingerprint`, `vidcull-matcher`) |
| Rust lint/format | `cargo fmt`, `cargo clippy --workspace --all-targets` |
| 성능 실측(벤치마크) | `cargo bench -p vidcull-fingerprint --bench fingerprint` — "느리다/빠르다"는 추정이 아니라 이 결과로 근거를 댄다 |
| 프론트 타입 체크 | `cd app && npm run check` |
| 프론트 유닛 테스트 | `cd app && npm test` |
| 프론트 E2E(Playwright) | `cd app && npm run e2e` (최초 1회 `npx playwright install chromium` 필요) |
| 개발 모드 실행 | `cd app && npm run tauri dev` |
| 데몬만 단독 실행 | `cargo run -p vidcull-daemon` |
| **인스톨러 전체 빌드** | 저장소 루트 `build-installer.bat` 실행(더블클릭 가능) — daemon 스테이징 → ffmpeg/디코드 사이드카 스테이징 → `npm run tauri build` 순서로 진행, 결과물은 `app/src-tauri/target/release/bundle/nsis/vidcull_<version>_x64-setup.exe` |

## 참고

- UI 변경은 코드 작성 후 실제로 `npm run tauri dev`나 `app/scripts/verify-*.mjs` 스크립트로 눈으로 확인한다 — 타입체크·유닛테스트 통과가 "기능이 실제로 동작함"을 보장하지 않는다.
- 버전을 올릴 때는 4개 파일을 함께 수정한다: 루트 `Cargo.toml`(`workspace.package.version`), `app/src-tauri/Cargo.toml`, `app/package.json`, `app/src-tauri/tauri.conf.json`.
- `app/scripts/verify-build-freshness.mjs`는 git 저장소가 아닌 환경(예: git 없이 배포되는 release 스냅샷)에서는 경고만 남기고 자동으로 건너뛴다 — `tauri.conf.json`의 `beforeBundleCommand`를 수동으로 지웠다 복원할 필요가 없다.
- ffmpeg는 `vendor/ffmpeg/MANIFEST.toml`에 SHA-256과 함께 핀돼 있다. 벤더 URL을 바꿀 때는 반드시 새 SHA-256을 재계산해 채운다.
- `rustfmt.toml`(edition 2024, max_width 100, 4-space), `clippy.toml`(msrv 1.85), `.editorconfig` 설정을 그대로 따른다.
