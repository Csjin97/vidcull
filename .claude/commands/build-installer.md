---
description: 인스톨러 전체 빌드(daemon+ffmpeg 스테이징 → tauri build)를 실행하고 결과를 보고한다
allowed-tools: Bash
---

저장소 루트의 `build-installer.bat`을 실행한다(`docs/commands.md` "인스톨러 전체 빌드" 참고).

1. `build-installer.bat`을 실행한다. 실시간 출력이 길 수 있으니 백그라운드로 실행하고 완료를 기다린다.
2. 종료 코드와 마지막 출력을 확인한다.
3. 성공하면 `app/src-tauri/target/release/bundle/nsis/vidcull_<version>_x64-setup.exe`가 실제로 생성됐는지 파일 존재로 확인하고 경로와 버전을 보고한다.
4. 실패하면 어느 단계(daemon 스테이징 / ffmpeg 사이드카 스테이징 / npm install / tauri build)에서 실패했는지 출력에서 찾아 원인과 함께 보고한다. 빌드 성공을 실행 성공으로 간주하지 않는다 — 파일 생성 여부까지 확인한다.
