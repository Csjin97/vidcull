<script lang="ts">
  import ProgressSparkline from "./ProgressSparkline.svelte";
  import Icon from "./Icon.svelte";
  import { formatBytes } from "$lib/model/format";
  import {
    completedFraction,
    currentFileLabel,
    isAnalyzingPartialClips,
    isFolderScanning,
    isScanning,
    partialCompleted,
    partialFailedTotal,
    partialOutstanding,
    partialSkippedBreakdown,
    partialSkippedTotal,
    partialTotal,
    totalTasks,
  } from "$lib/model/progress";
  import type { ProgressSnapshot } from "$lib/model/types";

  let {
    snapshot,
    throughput,
    etaSeconds,
    speedBytesPerSec = 0,
    speedKnown = false,
    stalled = false,
    reclaimableBytes,
    collapsed = false,
    ontoggle,
    watchedFolderCount = 0,
    lastActivityLabel = null,
    indexingEnabled = true,
    onindexingtoggle,
    paused = false,
  }: {
    snapshot: ProgressSnapshot | null;
    throughput: number[];
    etaSeconds: number | null;

    speedBytesPerSec?: number;

    speedKnown?: boolean;

    stalled?: boolean;
    reclaimableBytes: number;

    collapsed?: boolean;

    ontoggle?: () => void;

    watchedFolderCount?: number;

    lastActivityLabel?: string | null;

    indexingEnabled?: boolean;

    onindexingtoggle?: () => void;

    paused?: boolean;
  } = $props();

  const view = $derived.by(() => {
    const s = snapshot ?? { pending: 0, running: 0, done: 0, failed: 0 };
    return {
      total: totalTasks(s),
      fraction: completedFraction(s),
      scanning: isScanning(s),
      analyzingPartial: isAnalyzingPartialClips(s),
      partialOutstanding: partialOutstanding(s),
      partialCompleted: partialCompleted(s),
      partialTotal: partialTotal(s),
      partialSkippedTotal: partialSkippedTotal(s),
      partialSkippedBreakdown: partialSkippedBreakdown(s),
      partialFailed: partialFailedTotal(s),
      folderScanning: isFolderScanning(s),
      scanDiscovered: s.scanDiscovered ?? 0,
      partialKnown: s.partialDone !== undefined,
      pending: s.pending,
      running: s.running,
      done: s.done,
      failed: s.failed,
      cpuPermille: s.cpuUsagePermille ?? 0,
      rssBytes: s.rssBytes ?? 0,
    };
  });


  const currentFile = $derived(currentFileLabel(snapshot?.currentFiles));

  const percentLabel = $derived(`${Math.round(view.fraction * 100)}%`);


  const partialFillFraction = $derived(
    view.partialKnown && view.partialTotal > 0
      ? Math.min(1, Math.max(0, view.partialCompleted / view.partialTotal))
      : 0,
  );


  const cpuLabel = $derived(`${(view.cpuPermille / 10).toFixed(1)}%`);


  function formatEta(seconds: number): string {
    if (seconds < 60) return `${seconds}초`;
    if (seconds < 3600) {
      const m = Math.floor(seconds / 60);
      const s = seconds % 60;
      return s === 0 ? `${m}분` : `${m}분 ${s}초`;
    }
    const h = Math.floor(seconds / 3600);
    const m = Math.round((seconds % 3600) / 60);
    return m === 0 ? `${h}시간` : `${h}시간 ${m}분`;
  }


  const etaText = $derived(
    etaSeconds !== null
      ? formatEta(etaSeconds)
      : view.scanning && !speedKnown
        ? "측정 중"
        : "—",
  );


  const speedText = $derived(
    speedKnown
      ? `${formatBytes(speedBytesPerSec)}/s`
      : view.scanning
        ? "측정 중"
        : "—",
  );

  const stats = $derived([
    { key: "pending", label: "대기", value: view.pending, icon: "pending" },
    { key: "running", label: "처리 중", value: view.running, icon: "running" },
    { key: "done", label: "완료", value: view.done, icon: "done" },
    { key: "failed", label: "실패", value: view.failed, icon: "failed" },
  ]);
</script>

<section
  class="progress"
  class:progress--collapsed={collapsed}
  data-testid="progress-panel"
>
  <header class="progress__top">
    <button
      class="progress__collapse"
      type="button"
      data-testid="progress-toggle"
      aria-expanded={!collapsed}
      aria-label={collapsed ? "진행 패널 펼치기" : "진행 패널 접기"}
      onclick={() => ontoggle?.()}
    >
      {collapsed ? "▸" : "▾"}
    </button>
    <div class="progress__status">
      <div class="progress__status-line">
        <span
          class="progress__dot"
          class:progress__dot--live={view.scanning}
          aria-hidden="true"
        ></span>
        <span class="progress__state">
          {#if view.scanning}
            인덱싱 진행 중
          {:else if view.total > 0}
            인덱싱 완료
          {:else}
            대기 중
          {/if}
        </span>
        <span class="progress__pct">{percentLabel}</span>
        {#if view.total > 0}
          <span class="progress__count" data-testid="progress-count">
            {(view.done + view.failed).toLocaleString()} / {view.total.toLocaleString()}개
          </span>
        {/if}
      </div>
      {#if currentFile}
        
        <span
          class="progress__current"
          data-testid="progress-current"
          title={currentFile.title}
        >
          <span class="progress__current-icon" aria-hidden="true">▶</span>
          <span class="progress__current-name">{currentFile.name}</span>
          {#if currentFile.extra > 0}
            <span class="progress__current-extra"
              >외 {currentFile.extra.toLocaleString()}개</span
            >
          {/if}
        </span>
      {/if}
      {#if view.folderScanning}
        
        <span
          class="progress__scanning"
          data-testid="progress-folder-scan"
          aria-live="polite"
        >
          <span class="progress__scanning-icon" aria-hidden="true">📁</span>
          폴더 스캔 중{#if view.scanDiscovered > 0} · 발견 {view.scanDiscovered.toLocaleString()}개{/if}
        </span>
      {/if}
      {#if view.analyzingPartial}
        
        <span
          class="progress__partial"
          data-testid="progress-partial"
          aria-live="polite"
        >
          <span class="progress__partial-icon" aria-hidden="true">🔍</span>
          {#if view.partialKnown}
            부분클립 검사 {view.partialCompleted.toLocaleString()}/{view.partialTotal.toLocaleString()}
          {:else}
            부분클립 분석 중 · 남은 {view.partialOutstanding.toLocaleString()}개
          {/if}
        </span>
      {/if}
      {#if view.partialSkippedTotal > 0}
        
        <span
          class="progress__excluded"
          data-testid="progress-partial-excluded"
          aria-live="polite"
          title={view.partialSkippedBreakdown}
        >
          <span class="progress__excluded-icon" aria-hidden="true">⚠️</span>
          부분클립 제외 {view.partialSkippedTotal.toLocaleString()}개{#if view.partialSkippedBreakdown}
            <span class="progress__excluded-detail">({view.partialSkippedBreakdown})</span>
          {/if}
        </span>
      {/if}
      {#if view.partialFailed > 0}
        
        <span
          class="progress__failed-recheck"
          data-testid="progress-partial-failed"
          aria-live="polite"
        >
          <span class="progress__failed-recheck-icon" aria-hidden="true">⛔</span>
          실패/검증 필요 {view.partialFailed.toLocaleString()}개
        </span>
      {/if}
      
      <span class="progress__watcher" data-testid="progress-watcher">
        {#if watchedFolderCount > 0}
          <span class="progress__watcher-pill" data-testid="progress-watcher-active">
            감시 중 ({watchedFolderCount}개 폴더)
          </span>
        {:else}
          <span class="progress__watcher-pill progress__watcher-pill--none" data-testid="progress-watcher-none">
            감시 폴더 없음
          </span>
        {/if}
        {#if lastActivityLabel !== null}
          <span class="progress__watcher-activity" data-testid="progress-watcher-activity">
            최근 인덱싱 활동: {lastActivityLabel}
          </span>
        {/if}
      </span>
    </div>
    
    {#if onindexingtoggle}
      <button
        class="progress__pause"
        type="button"
        aria-pressed={!indexingEnabled}
        data-testid="progress-pause-btn"
        onclick={() => onindexingtoggle?.()}
      >
        {indexingEnabled ? "검사 일시정지" : "검사 재개"}
      </button>
    {/if}
    <div class="progress__reclaim">
      <span class="progress__reclaim-value">{formatBytes(reclaimableBytes)}</span>
      <span class="eyebrow">정리 시 확보 가능</span>
    </div>
  </header>

  <div
    class="progress__bar"
    role="progressbar"
    aria-valuenow={Math.round(view.fraction * 100)}
    aria-valuemin="0"
    aria-valuemax="100"
    aria-label="인덱싱 진행률"
  >
    <div class="progress__fill" style="width: {view.fraction * 100}%"></div>
    {#if view.analyzingPartial}
      
      {#if view.partialKnown}
        <div
          class="progress__partial-seg progress__partial-seg--ratio"
          data-testid="progress-partial-seg"
          data-partial-fill="ratio"
          style="width: {partialFillFraction * 100}%"
          title="부분클립 검사 {view.partialCompleted.toLocaleString()}/{view.partialTotal.toLocaleString()}"
        ></div>
      {:else}
        <div
          class="progress__partial-seg progress__partial-seg--stripes"
          data-testid="progress-partial-seg"
          data-partial-fill="stripes"
          title="부분클립 분석 중 · 남은 {view.partialOutstanding.toLocaleString()}개"
        ></div>
      {/if}
    {/if}
  </div>

  {#if !collapsed}
  <div class="progress__body">
    <dl class="progress__stats">
      {#each stats as stat (stat.key)}
        <div class="stat stat--{stat.key}" title={stat.label}>
          <dd class="stat__value">{stat.value.toLocaleString()}</dd>
          <dt class="stat__label">{stat.label}</dt>
          <Icon name={stat.icon} class="stat__icon" />
        </div>
      {/each}
    </dl>

    <div class="progress__chart">
      <div class="progress__chart-head">
        <span class="eyebrow">처리 속도</span>
        {#if stalled}
          <span class="progress__stall" data-testid="progress-stall"
            >⏳ 지연 파일 처리 중</span
          >
        {/if}
        <span class="progress__eta" data-testid="progress-eta">
          {#if paused && view.scanning}
            일시정지
          {:else}
            남은 시간 {etaText}
          {/if}
        </span>
      </div>
      
      <div class="progress__chart-row">
        <div class="progress__spark">
          <ProgressSparkline values={throughput} />
        </div>
        <dl class="progress__metrics" data-testid="progress-metrics">
          <div class="metric" title="데몬 CPU 사용률">
            <dt class="metric__label">CPU</dt>
            <dd class="metric__value" data-testid="metric-cpu">{cpuLabel}</dd>
          </div>
          <div class="metric" title="데몬 메모리 사용량 (RSS)">
            <dt class="metric__label">메모리</dt>
            <dd class="metric__value" data-testid="metric-rss">
              {formatBytes(view.rssBytes)}
            </dd>
          </div>
          <div class="metric" title="처리 속도 (처리 중 MB/s)">
            <dt class="metric__label">속도</dt>
            <dd class="metric__value" data-testid="metric-throughput">
              {speedText}
            </dd>
          </div>
        </dl>
      </div>
    </div>
  </div>
  {/if}
</section>

<style>
  .progress {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
    padding: var(--space-sm) var(--space-md);
    background: var(--color-canvas-elevated);
    border-bottom: 1px solid var(--color-hairline);
    
    container-type: inline-size;
  }
  
  :global(.stat__icon) {
    display: none;
    font-size: 1.05rem;
    color: var(--color-muted);
  }
  @container (max-width: 540px) {
    .stat__label {
      display: none;
    }
    :global(.stat__icon) {
      display: inline-flex;
    }
  }
  .progress__top {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: var(--space-sm);
  }
  .progress__collapse {
    align-self: center;
    margin-right: var(--space-xs);
    padding: 0 var(--space-xxs);
    background: transparent;
    border: none;
    color: var(--color-muted);
    font-size: var(--type-title-md-size);
    line-height: 1;
    cursor: pointer;
  }
  
  .progress__pause {
    flex: 0 0 auto;
    align-self: center;
    padding: var(--space-xxs) var(--space-xs);
    background: transparent;
    border: 1px solid var(--color-hairline);
    border-radius: var(--radius-sm);
    color: var(--color-muted);
    font-size: var(--type-caption-size);
    line-height: 1.4;
    cursor: pointer;
    white-space: nowrap;
  }
  .progress__pause:hover {
    color: var(--color-ink);
    border-color: var(--color-primary);
  }
  .progress__pause[aria-pressed="true"] {
    color: var(--color-primary);
    border-color: var(--color-primary);
  }
  .progress__status {
    display: flex;
    flex-direction: column;
    gap: 2px;
    

    min-width: 0;
    flex: 1 1 auto;
  }
  .progress__status-line {
    display: flex;
    align-items: baseline;
    gap: var(--space-xs);
  }
  
  .progress__current {
    display: flex;
    align-items: baseline;
    gap: var(--space-xxs);
    min-width: 0;
    max-width: 100%;
    font-size: var(--type-body-sm-size);
    color: var(--color-muted);
  }
  .progress__current-icon {
    color: var(--color-primary);
    font-size: 0.7em;
    flex: 0 0 auto;
  }
  .progress__current-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
    color: var(--color-body);
  }
  .progress__current-extra {
    flex: 0 0 auto;
    color: var(--color-muted);
    font-variant-numeric: tabular-nums;
  }
  
  .progress__partial {
    display: flex;
    align-items: baseline;
    gap: var(--space-xxs);
    font-size: var(--type-body-sm-size);
    color: var(--color-muted);
  }
  .progress__partial-icon {
    font-size: 0.75em;
    flex: 0 0 auto;
  }
  
  .progress__scanning {
    display: flex;
    align-items: baseline;
    gap: var(--space-xxs);
    font-size: var(--type-body-sm-size);
    color: var(--color-muted);
  }
  .progress__scanning-icon {
    font-size: 0.75em;
    flex: 0 0 auto;
  }
  


  .progress__excluded {
    display: flex;
    align-items: baseline;
    gap: var(--space-xxs);
    font-size: var(--type-body-sm-size);
    color: var(--color-accent-yellow);
    font-weight: var(--font-weight-strong);
  }
  .progress__excluded-icon {
    font-size: 0.75em;
    flex: 0 0 auto;
  }
  .progress__excluded-detail {
    color: var(--color-muted-soft);
    font-weight: normal;
  }
  



  .progress__failed-recheck {
    display: flex;
    align-items: baseline;
    gap: var(--space-xxs);
    font-size: var(--type-body-sm-size);
    color: var(--color-danger, #c81d25);
    font-weight: var(--font-weight-strong);
  }
  .progress__failed-recheck-icon {
    font-size: 0.75em;
    flex: 0 0 auto;
  }
  
  .progress__watcher {
    display: flex;
    align-items: baseline;
    gap: var(--space-xs);
    flex-wrap: wrap;
  }
  .progress__watcher-pill {
    font-size: var(--type-caption-size);
    color: var(--color-primary);
    font-weight: var(--font-weight-strong);
  }
  .progress__watcher-pill--none {
    color: var(--color-muted);
    font-weight: normal;
  }
  .progress__watcher-activity {
    font-size: var(--type-caption-size);
    color: var(--color-muted);
  }
  .progress__dot {
    width: 8px;
    height: 8px;
    border-radius: var(--radius-full);
    background: var(--color-muted);
    align-self: center;
  }
  .progress__dot--live {
    background: var(--color-primary);
    animation: pulse 1.4s ease-in-out infinite;
  }
  @keyframes pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.3;
    }
  }
  .progress__state {
    font-size: var(--type-title-sm-size);
    color: var(--color-ink);
  }
  .progress__pct {
    font-size: var(--type-title-md-size);
    font-weight: var(--font-weight-strong);
    color: var(--color-ink);
  }
  .progress__count {
    color: var(--color-muted);
    font-size: var(--type-body-sm-size);
    font-variant-numeric: tabular-nums;
  }
  .progress__reclaim {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
  }
  .progress__reclaim-value {
    color: var(--color-primary);
    font-size: var(--type-display-md-size);
    font-weight: var(--font-weight-strong);
    line-height: 1;
  }
  .progress__bar {
    position: relative;
    height: 6px;
    background: var(--color-canvas);
    border: 1px solid var(--color-hairline);
    overflow: hidden;
  }
  .progress__fill {
    height: 100%;
    background: var(--color-primary);
    transition: width 240ms ease;
  }
  


  .progress__partial-seg {
    position: absolute;
    top: 0;
    bottom: 0;
    left: 0;
  }
  

  .progress__partial-seg--ratio {
    background: var(--color-semantic-info);
    opacity: 0.55;
    transition: width 240ms ease;
  }
  

  .progress__partial-seg--stripes {
    right: 0;
    background-image: repeating-linear-gradient(
      -45deg,
      var(--color-semantic-info) 0,
      var(--color-semantic-info) 6px,
      transparent 6px,
      transparent 12px
    );
    opacity: 0.5;
    animation: partial-stripes 1s linear infinite;
  }
  @keyframes partial-stripes {
    from {
      background-position: 0 0;
    }
    to {
      background-position: 17px 0;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .progress__partial-seg--stripes {
      animation: none;
    }
  }
  .progress__body {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(220px, 1.1fr);
    gap: var(--space-md);
    align-items: center;
  }
  .progress__stats {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    
    gap: var(--space-xxs);
    margin: 0;
  }
  .stat {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .stat__value {
    margin: 0;
    font-size: var(--type-title-md-size);
    font-weight: var(--font-weight-strong);
    color: var(--color-ink);
    font-variant-numeric: tabular-nums;
  }
  .stat__label {
    font-size: var(--type-caption-size);
    color: var(--color-muted);
  }
  .stat--failed .stat__value {
    color: var(--color-semantic-warning);
  }
  .progress__chart {
    display: flex;
    flex-direction: column;
    gap: var(--space-xxxs);
    min-width: 0;
  }
  .progress__chart-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
  }
  .progress__eta {
    font-size: var(--type-caption-size);
    color: var(--color-body);
  }
  .progress__stall {
    font-size: var(--type-caption-size);
    color: var(--color-muted);
    white-space: nowrap;
  }
  

  .progress__chart-row {
    display: flex;
    gap: var(--space-md);
    align-items: center;
    min-width: 0;
  }
  .progress__spark {
    flex: 1 1 auto;
    height: 48px;
    
    max-width: 200px;
    min-width: 0;
  }
  .progress__metrics {
    display: flex;
    flex-direction: column;
    gap: var(--space-xxs);
    margin: 0;
    flex: 0 0 auto;
    justify-content: center;
  }
  .metric {
    display: flex;
    align-items: baseline;
    gap: var(--space-xxs);
    min-width: 0;
  }
  .metric__label {
    font-size: var(--type-caption-size);
    color: var(--color-muted);
  }
  .metric__value {
    margin: 0;
    font-size: var(--type-body-sm-size);
    color: var(--color-ink);
    font-variant-numeric: tabular-nums;
  }
</style>
