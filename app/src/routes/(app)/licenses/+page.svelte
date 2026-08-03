<script lang="ts">
  // Fetched lazily (not bundled via ?raw) — the full notices text is ~650KB
  // and almost nobody opens this <details>, so importing it at build time
  // just bloats this route's JS chunk for no benefit.
  let notices = $state<string | null>(null);
  let noticesLoading = $state(false);
  let noticesError = $state("");

  async function loadNotices(): Promise<void> {
    if (notices !== null || noticesLoading) return;
    noticesLoading = true;
    noticesError = "";
    try {
      const res = await fetch("/third-party-notices.txt");
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      notices = await res.text();
    } catch (err) {
      noticesError = `전문을 불러오지 못했습니다: ${String(err)}`;
    } finally {
      noticesLoading = false;
    }
  }

  function onDetailsToggle(e: Event): void {
    if ((e.currentTarget as HTMLDetailsElement).open) {
      void loadNotices();
    }
  }

  type Item = { name: string; license: string; note?: string };
  type Group = { title: string; blurb?: string; items: Item[] };

  const app: Item = {
    name: "vidcull",
    license: "PolyForm Noncommercial 1.0.0",
    note: "소스 공개·비상업 전용 라이선스. OSI 오픈소스가 아니며, 상업적 이용·수익화는 허용되지 않습니다.",
  };

  const groups: Group[] = [
    {
      title: "별도 프로세스로 호출되는 도구",
      blurb:
        "vidcull에 링크(정적/동적)되지 않고, 독립 실행 파일을 CLI 경계로 호출합니다(mere aggregation).",
      items: [
        {
          name: "FFmpeg / ffprobe (BtbN GPL 빌드)",
          license: "GPL-3.0",
          note:
            "폴백 코덱(AV1 / VP9 / MPEG-2 / 손상 파일) 디코드 전용. 인스톨러에 번들되지 않고, 설치 시 업스트림(BtbN)에서 다운로드됩니다 — 재배포가 아니라 사용자의 기기가 직접 취득합니다. 네이티브 H.264 / H.265 경로는 ffmpeg 없이 동작합니다.",
        },
        {
          name: "Microsoft Edge WebView2 Runtime",
          license: "Microsoft Software License Terms (독점)",
          note: "앱 UI 렌더링용 시스템 런타임. 설치 시 부트스트래퍼로 취득됩니다.",
        },
      ],
    },
    {
      title: "프론트엔드",
      items: [
        { name: "Svelte", license: "MIT" },
        { name: "SvelteKit (@sveltejs/kit)", license: "MIT" },
        { name: "Tauri (@tauri-apps/api · tauri)", license: "Apache-2.0 OR MIT" },
        { name: "bits-ui", license: "MIT" },
        { name: "Inter 글꼴 (@fontsource/inter)", license: "SIL Open Font License 1.1" },
      ],
    },
    {
      title: "Rust 라이브러리 (핵심)",
      blurb:
        "전체 220여 개 의존성 — 대부분 허용형(MIT 또는 Apache-2.0)입니다. 주목할 항목과 핵심 크레이트만 추렸습니다.",
      items: [
        { name: "mp4parse", license: "MPL-2.0", note: "약한 카피레프트(파일 단위)." },
        { name: "rusqlite", license: "MIT", note: "SQLite 엔진 자체는 퍼블릭 도메인." },
        { name: "matroska-demuxer", license: "Zlib OR MIT OR Apache-2.0" },
        { name: "wide (정수 SIMD)", license: "Zlib OR Apache-2.0 OR MIT" },
        { name: "image", license: "MIT OR Apache-2.0" },
        { name: "rayon", license: "MIT OR Apache-2.0" },
        { name: "serde · serde_json", license: "MIT OR Apache-2.0" },
        { name: "tokio", license: "MIT" },
        { name: "tracing", license: "MIT" },
        { name: "blake3", license: "CC0-1.0 OR Apache-2.0" },
        { name: "notify", license: "CC0-1.0" },
        { name: "anyhow · thiserror · postcard · ahash …", license: "MIT OR Apache-2.0" },
      ],
    },
  ];
</script>

<div class="licenses">
  <header class="licenses__head">
    <h1 class="licenses__title">라이선스</h1>
    <p class="licenses__intro">
      vidcull과 vidcull이 사용하는 제3자 라이브러리의 라이선스입니다.
    </p>
  </header>

  <section class="lic-card lic-card--app">
    <h2>이 소프트웨어</h2>
    <div class="lic-row">
      <span class="lic-name">{app.name}</span>
      <span class="lic-id">{app.license}</span>
    </div>
    {#if app.note}<p class="lic-note">{app.note}</p>{/if}
  </section>

  {#each groups as group (group.title)}
    <section class="lic-card">
      <h2>{group.title}</h2>
      {#if group.blurb}<p class="lic-blurb">{group.blurb}</p>{/if}
      <ul class="lic-list">
        {#each group.items as item (item.name)}
          <li class="lic-item">
            <div class="lic-row">
              <span class="lic-name">{item.name}</span>
              <span class="lic-id">{item.license}</span>
            </div>
            {#if item.note}<p class="lic-note">{item.note}</p>{/if}
          </li>
        {/each}
      </ul>
    </section>
  {/each}

  <details class="lic-full" ontoggle={onDetailsToggle}>
    <summary>전체 제3자 라이선스 전문 보기 (THIRD-PARTY NOTICES · 257개 구성요소)</summary>
    {#if noticesLoading}
      <p class="lic-note">불러오는 중…</p>
    {:else if noticesError}
      <p class="lic-note lic-note--err">{noticesError}</p>
    {:else if notices !== null}
      <pre class="lic-full__text">{notices}</pre>
    {/if}
  </details>

  <p class="licenses__foot">
    위 전문은 <code>scripts/gen-third-party-notices.py</code>로 자동 생성되며 인스톨러에도 동봉됩니다.
  </p>
</div>

<style>
  .licenses {
    height: 100vh;
    overflow-y: auto;
    padding: var(--space-lg) var(--space-xl);
    display: flex;
    flex-direction: column;
    gap: var(--space-md);
  }
  .licenses__head {
    display: flex;
    flex-direction: column;
    gap: var(--space-xxs);
  }
  .licenses__title {
    font-size: var(--type-title-lg-size, 1.5rem);
    font-weight: var(--font-weight-strong);
    margin: 0;
  }
  .licenses__intro {
    color: var(--color-muted);
    font-size: var(--type-body-md-size);
    margin: 0;
  }
  .lic-card {
    background: var(--color-canvas-elevated);
    border: 1px solid var(--color-hairline);
    border-radius: var(--radius-md, 8px);
    padding: var(--space-md);
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
  }
  .lic-card h2 {
    font-size: var(--type-title-md-size, 1.1rem);
    font-weight: var(--font-weight-strong);
    margin: 0;
  }
  .lic-card--app {
    border-left: 2px solid var(--color-primary);
  }
  .lic-blurb {
    color: var(--color-muted);
    font-size: var(--type-caption-size);
    margin: 0;
  }
  .lic-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-sm);
  }
  .lic-item {
    border-top: 1px solid var(--color-hairline);
    padding-top: var(--space-sm);
  }
  .lic-item:first-child {
    border-top: none;
    padding-top: 0;
  }
  .lic-row {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-md);
    flex-wrap: wrap;
  }
  .lic-name {
    color: var(--color-ink);
    font-weight: var(--font-weight-strong);
  }
  .lic-id {
    color: var(--color-body);
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: var(--type-caption-size);
    white-space: nowrap;
  }
  .lic-note {
    color: var(--color-muted);
    font-size: var(--type-caption-size);
    margin: var(--space-xxs) 0 0;
  }
  .lic-note--err {
    color: var(--color-semantic-warning, var(--color-muted));
  }
  .lic-full {
    background: var(--color-canvas-elevated);
    border: 1px solid var(--color-hairline);
    border-radius: var(--radius-md, 8px);
    padding: var(--space-sm) var(--space-md);
  }
  .lic-full > summary {
    cursor: pointer;
    color: var(--color-body);
    font-weight: var(--font-weight-strong);
    font-size: var(--type-body-md-size);
  }
  .lic-full > summary:hover {
    color: var(--color-ink);
  }
  .lic-full__text {
    margin: var(--space-sm) 0 0;
    max-height: 60vh;
    overflow: auto;
    white-space: pre-wrap;
    word-break: break-word;
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: var(--type-caption-size);
    line-height: 1.5;
    color: var(--color-body);
  }
  .licenses__foot {
    color: var(--color-muted);
    font-size: var(--type-caption-size);
    margin: 0 0 var(--space-lg);
  }
  .licenses__foot code {
    font-family: var(--font-mono, ui-monospace, monospace);
  }
</style>
