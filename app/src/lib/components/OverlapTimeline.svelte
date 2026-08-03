<script lang="ts">
  import { formatDuration } from "$lib/model/format";
  import { laneCount, layoutOverlaps } from "$lib/model/timeline";
  import type { ClipOverlap } from "$lib/model/types";

  let {
    sourceDurationMs,
    overlaps,
    sourceLabel = "원본",
    headingLabel = "부분 클립 겹침",
    notice = undefined,
  }: {
    sourceDurationMs: number;
    overlaps: ClipOverlap[];
    sourceLabel?: string;

    headingLabel?: string;

    notice?: string;
  } = $props();

  const LANE_HEIGHT = 18; 

  const segments = $derived(layoutOverlaps(sourceDurationMs, overlaps));
  const lanes = $derived(laneCount(segments));
  const ticks = $derived(
    [0, 0.25, 0.5, 0.75, 1].map((f) => ({
      fraction: f,
      label: formatDuration(sourceDurationMs * f),
    })),
  );
</script>

<section class="timeline" data-testid="overlap-timeline">
  <header class="timeline__head">
    <span class="eyebrow">{headingLabel}</span>
    <span class="timeline__source" title={sourceLabel}>{sourceLabel}</span>
  </header>

  {#if notice}
    <p class="timeline__notice" data-testid="overlap-notice">{notice}</p>
  {/if}

  {#if segments.length === 0}
    {#if !notice}
      <p class="timeline__empty">이 원본과 겹치는 부분 클립이 없습니다.</p>
    {/if}
  {:else}
    <div
      class="timeline__lanes"
      style="height: {Math.max(1, lanes) * LANE_HEIGHT}px"
    >
      {#each ticks as tick (tick.fraction)}
        <span class="timeline__grid" style="left: {tick.fraction * 100}%"></span>
      {/each}
      {#each segments as seg (seg.clipFileId)}
        <div
          class="timeline__bar"
          class:timeline__bar--intro-outro={seg.introOutro}
          data-testid="overlap-bar"
          style="
            left: {seg.startFraction * 100}%;
            width: {Math.max(0.6, (seg.endFraction - seg.startFraction) * 100)}%;
            top: {seg.lane * LANE_HEIGHT}px;
            opacity: {0.4 + seg.coverage * 0.6};
          "
          title={`영상 #${seg.clipFileId} · ${formatDuration(seg.startMs)}–${formatDuration(seg.endMs)} · 일치율 ${Math.round(seg.coverage * 100)}%${seg.introOutro ? " · 인트로/아웃트로 의심" : ""}`}
        ></div>
      {/each}
    </div>

    <div class="timeline__ruler">
      {#each ticks as tick (tick.fraction)}
        <span
          class="timeline__tick"
          class:timeline__tick--end={tick.fraction === 1}>{tick.label}</span
        >
      {/each}
    </div>

    <ul class="timeline__list" aria-label="겹침 구간 목록">
      {#each [...segments].sort((a, b) => a.startMs - b.startMs) as seg (seg.clipFileId)}
        <li class="timeline__list-item" data-testid="overlap-range">
          <span class="timeline__list-id">영상 #{seg.clipFileId}</span>
          <span class="timeline__list-range"
            >{formatDuration(seg.startMs)}–{formatDuration(seg.endMs)}</span
          >
          {#if seg.introOutro}
            <span class="timeline__list-badge" data-testid="overlap-intro-outro-badge"
              >인트로/아웃트로 의심</span
            >
          {/if}
          <span class="timeline__list-coverage"
            >일치율 {Math.round(seg.coverage * 100)}%</span
          >
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .timeline {
    display: flex;
    flex-direction: column;
    gap: var(--space-xxs);
    padding: var(--space-xs);
    background: var(--color-surface-card);
    border: 1px solid var(--color-hairline);
  }
  .timeline__head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-xs);
  }
  .timeline__source {
    font-size: var(--type-caption-size);
    color: var(--color-body);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .timeline__empty {
    margin: 0;
    color: var(--color-muted);
    font-size: var(--type-body-sm-size);
  }
  .timeline__notice {
    margin: 0;
    color: var(--color-muted);
    font-size: var(--type-caption-size);
  }
  .timeline__lanes {
    position: relative;
    width: 100%;
    background: var(--color-canvas);
    border: 1px solid var(--color-hairline);
    overflow: hidden;
  }
  .timeline__grid {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 1px;
    background: var(--color-hairline);
    opacity: 0.5;
  }
  .timeline__bar {
    position: absolute;
    height: 12px;
    margin-top: 3px;
    background: var(--color-primary);
    border-radius: var(--radius-none);
  }
  


  .timeline__bar--intro-outro {
    background: var(--color-semantic-warning);
  }
  .timeline__ruler {
    display: flex;
    justify-content: space-between;
  }
  .timeline__tick {
    font-size: var(--type-caption-size);
    color: var(--color-muted);
    font-variant-numeric: tabular-nums;
  }
  .timeline__tick--end {
    text-align: right;
  }
  .timeline__list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-xxs);
  }
  .timeline__list-item {
    display: flex;
    align-items: baseline;
    gap: var(--space-xs);
    font-size: var(--type-caption-size);
    font-variant-numeric: tabular-nums;
  }
  .timeline__list-id {
    color: var(--color-muted);
    flex-shrink: 0;
  }
  .timeline__list-range {
    color: var(--color-body);
    font-weight: 500;
  }
  .timeline__list-coverage {
    color: var(--color-muted);
    margin-left: auto;
  }
  


  .timeline__list-badge {
    padding: 0 var(--space-xxs);
    color: var(--color-semantic-warning);
    border: 1px solid var(--color-semantic-warning);
    white-space: nowrap;
  }
</style>
