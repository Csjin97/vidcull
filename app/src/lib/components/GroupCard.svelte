<!--
  [변경 이력 (Changelog)]
  - 2026-08-03 : 빈 그룹 렌더링 시 width 접근 오류 방지
-->
<script lang="ts">
  import Thumbnail from "./Thumbnail.svelte";
  import TrustBadge from "./TrustBadge.svelte";
  import { membersByQuality } from "$lib/model/best-copy";
  import { formatBytes, resolutionLabel } from "$lib/model/format";
  import type { DuplicateGroup } from "$lib/model/types";

  let {
    group,
    selected = false,
    onselect,
  }: {
    group: DuplicateGroup;
    selected?: boolean;
    onselect?: (group: DuplicateGroup) => void;
  } = $props();

  const ordered = $derived(membersByQuality(group));
  const best = $derived(ordered[0]);
  const reclaimable = $derived(
    ordered.slice(1).reduce((sum, f) => sum + f.sizeBytes, 0),
  );
  const thumbs = $derived(ordered.slice(0, 4));
  const overflow = $derived(group.members.length - thumbs.length);
</script>

<button
  class="card"
  class:card--selected={selected}
  type="button"
  aria-pressed={selected}
  onclick={() => onselect?.(group)}
>
  <div class="card__thumbs">
    {#each thumbs as member (member.fileId)}
      <Thumbnail
        src={member.thumbnailUrl}
        alt={`그룹 ${group.groupId} · 파일 ${member.fileId}`}
      />
    {/each}
    {#if overflow > 0}
      <span class="card__more">+{overflow}</span>
    {/if}
  </div>

  <div class="card__meta">
    <div class="card__top">
      <TrustBadge trust={group.trust} />
      <span class="card__count">{group.members.length}개 파일</span>
    </div>
    <div class="card__title">
      {best ? `${resolutionLabel(best.width, best.height)} · 최적 사본 기준` : "파일 정보 갱신 중"}
    </div>
    <div class="card__sub">정리 시 회수 {formatBytes(reclaimable)}</div>
  </div>

  <span class="card__chevron" aria-hidden="true">›</span>
</button>

<style>
  .card {
    display: flex;
    align-items: center;
    gap: var(--space-sm);
    width: 100%;
    height: 100%;
    padding: var(--space-xs) var(--space-sm);
    background: var(--color-surface-card);
    border: 1px solid var(--color-hairline);
    border-radius: var(--radius-none);
    color: var(--color-ink);
    text-align: left;
    cursor: pointer;
    transition: border-color 120ms ease, background-color 120ms ease;
  }
  .card:hover {
    border-color: var(--color-muted);
  }
  .card--selected {
    border-color: var(--color-primary);
  }
  .card__thumbs {
    position: relative;
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: var(--space-xxxs);
    


    width: clamp(140px, 34%, 360px);
    min-width: 0;
    flex: 0 1 auto;
  }
  .card__more {
    position: absolute;
    right: var(--space-xxxs);
    bottom: var(--space-xxxs);
    padding: 0 var(--space-xxs);
    background: var(--color-canvas);
    color: var(--color-ink);
    font-size: var(--type-caption-size);
    border: 1px solid var(--color-hairline);
  }
  .card__meta {
    display: flex;
    flex-direction: column;
    gap: var(--space-xxxs);
    flex: 1 1 auto;
    min-width: 0;
  }
  .card__top {
    display: flex;
    align-items: center;
    gap: var(--space-xs);
    min-width: 0;
  }
  .card__count {
    color: var(--color-body);
    font-size: var(--type-body-sm-size);
    white-space: nowrap;
  }
  


  .card__title {
    font-size: var(--type-title-sm-size);
    font-weight: var(--font-weight-display);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .card__sub {
    color: var(--color-body);
    font-size: var(--type-body-sm-size);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .card__chevron {
    color: var(--color-muted);
    font-size: var(--type-display-md-size);
    flex: 0 0 auto;
  }
</style>
