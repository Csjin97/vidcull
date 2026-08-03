#!/usr/bin/env python3
"""Generate app/static/third-party-notices.txt — the THIRD-PARTY attribution
shown in the app's 라이선스 tab and shipped with the installer.

Lives under app/static/ (a plain static asset, fetched lazily by the licenses
page) rather than app/src/lib/ so its ~650KB doesn't get bundled into the
licenses route's JS chunk.

vidcull keeps its own source closed/PolyForm; this file is the *attribution*
(copyright + license notices) that permissive licenses (MIT/Apache/BSD/Zlib/
ISC/OFL/…) require when distributing binaries. It does NOT disclose vidcull's
source — see docs/release-packaging.md.

Regenerate (run from repo root, both tools one-time installable):
    cargo install cargo-bundle-licenses          # once
    cargo bundle-licenses --format json --output target/rust-licenses.json
    npx --yes license-checker --start app --production --json > target/npm-licenses.json
    python scripts/gen-third-party-notices.py

Inputs are read as UTF-8; the output is written UTF-8 (this machine's default
console codec is cp949, so explicit encodings are mandatory).
"""
import io
import json
import os

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RUST_JSON = os.path.join(ROOT, "target", "rust-licenses.json")
NPM_JSON = os.path.join(ROOT, "target", "npm-licenses.json")
OUT = os.path.join(ROOT, "app", "static", "third-party-notices.txt")

entries = []

rust = json.load(open(RUST_JSON, encoding="utf-8"))
for lib in rust["third_party_libraries"]:
    texts = [
        ((l.get("license") or "?"), (l.get("text") or "").strip())
        for l in lib.get("licenses", [])
    ]
    entries.append(
        (
            "crate",
            lib["package_name"],
            lib.get("package_version", ""),
            lib.get("license", "") or "",
            texts,
        )
    )

npm = json.load(open(NPM_JSON, encoding="utf-8"))
for key, info in npm.items():
    name, _, ver = key.rpartition("@")  # scoped names keep their leading '@'
    if name in ("vidcull-ui", ""):  # our own app package — not third-party
        continue
    lic = info.get("licenses", "")
    lic = lic if isinstance(lic, str) else " / ".join(lic)
    text = ""
    lf = info.get("licenseFile")
    if lf and os.path.isfile(lf):
        try:
            text = open(lf, encoding="utf-8", errors="replace").read().strip()
        except OSError:
            text = ""
    entries.append(("npm", name, ver, lic, [(lic, text)]))

groups = {}
for eco, name, ver, _expr, texts in entries:
    for lic_id, text in texts:
        if not text:
            continue
        g = groups.setdefault(text, {"id": lic_id, "pkgs": set()})
        g["pkgs"].add((eco, name, ver))

flat = sorted({(eco, name, ver, expr) for eco, name, ver, expr, _ in entries})
n_crate = sum(1 for e in flat if e[0] == "crate")
n_npm = sum(1 for e in flat if e[0] == "npm")

buf = io.StringIO()
w = buf.write
bar = "=" * 76
w("vidcull — 제3자 라이선스 고지 (THIRD-PARTY NOTICES)\n" + bar + "\n\n")
w("vidcull 본체는 PolyForm Noncommercial 1.0.0(소스 공개·비상업) 라이선스입니다.\n")
w("아래는 vidcull이 사용하는 제3자 구성요소의 저작권·라이선스 고지입니다(출처 표기\n")
w("의무 충족용 — vidcull 소스 공개와 무관). scripts/gen-third-party-notices.py로 자동\n")
w("생성합니다(cargo-bundle-licenses + license-checker).\n\n")

w("-" * 76 + "\n별도 프로세스로 호출 / 설치 시 취득되는 구성요소\n" + "-" * 76 + "\n")
w(
    "FFmpeg (ffmpeg.exe / ffprobe.exe, libav: avcodec·avformat·avutil·swscale·\n"
    "  swresample) — LGPL-2.1+. GPL 컴포넌트(x264/x265 등)는 포함하지 않습니다\n"
    "  (--enable-gpl 미사용).\n"
    "  · vidcull 본체 데몬은 libav를 링크하지 않고 ffmpeg.exe를 별도 프로세스로 CLI\n"
    "    호출합니다(mere aggregation).\n"
    "  · 부분클립 디코드 가속 sidecar(vidcull-decode-sidecar)가 동봉된 경우, 이 sidecar는\n"
    "    FFmpeg libav 공유 라이브러리를 **동적 링크**합니다. 동적 링크되는 공유 라이브러리는\n"
    "    사용자가 교체/재링크할 수 있으며(LGPL 재링크 요건), 대응 소스는 아래 업스트림에서\n"
    "    입수 가능합니다.\n"
    "  소스: https://github.com/BtbN/FFmpeg-Builds · https://ffmpeg.org\n"
)
w(
    "Microsoft Edge WebView2 Runtime — Microsoft Software License Terms(독점).\n"
    "  설치 시 부트스트래퍼로 취득되는 시스템 런타임.\n\n"
)

w("-" * 76 + "\n구성요소 목록 — crates(Rust) %d · npm %d\n" % (n_crate, n_npm) + "-" * 76 + "\n")
for eco, name, ver, expr in flat:
    w(f"  [{eco}] {name} {ver} — {expr}\n")

w("\n" + "-" * 76 + "\n라이선스 전문\n" + "-" * 76 + "\n")
for text, g in sorted(groups.items(), key=lambda kv: (-len(kv[1]["pkgs"]), kv[1]["id"])):
    pkgs = sorted(g["pkgs"])
    w("\n" + "#" * 76 + "\n")
    w(f"# 다음 {len(pkgs)}개 구성요소에 적용 ({g['id']}):\n")
    for eco, name, ver in pkgs:
        w(f"#   [{eco}] {name} {ver}\n")
    w("#" * 76 + "\n")
    w(text + "\n")

open(OUT, "w", encoding="utf-8").write(buf.getvalue())
print(
    f"wrote {OUT}: {len(buf.getvalue())} bytes, "
    f"{len(flat)} packages ({n_crate} crates + {n_npm} npm), "
    f"{len(groups)} unique license texts"
)
