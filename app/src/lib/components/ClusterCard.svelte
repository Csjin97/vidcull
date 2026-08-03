<!--
  [변경 이력 (Changelog)]
  - 2026-08-03 : 빈 클러스터 렌더링 시 width 접근 오류 방지
-->
<script lang="ts">
  import Thumbnail from "./Thumbnail.svelte";
  import TrustBadge from "./TrustBadge.svelte";
  import { membersByQuality } from "$lib/model/best-copy";
  import { clusterAsGroup, memberTrustLevels } from "$lib/model/cluster";
  import { formatBytes, resolutionLabel } from "$lib/model/format";
  import type { ContentCluster } from "$lib/model/types";

  let {
    cluster,
    selected = false,
    checked = false,
    onselect,
    onchecktoggle,
    fetchThumbnail,
  }: {
    cluster: ContentCluster;
    selected?: boolean;
    checked?: boolean;
    onselect?: (cluster: ContentCluster) => void;
    onchecktoggle?: (cluster: ContentCluster) => void;
    fetchThumbnail?: (fileId: number) => Promise<string | null>;
  } = $props();

  const ordered = $derived(membersByQuality(clusterAsGroup(cluster)));
  const best = $derived(ordered[0]);
  const reclaimable = $derived(
    ordered.slice(1).reduce((sum, f) => sum + f.sizeBytes, 0),
  );
  const thumbs = $derived(ordered.slice(0, 4));
  const overflow = $derived(cluster.members.length - thumbs.length);
  const badges = $derived(memberTrustLevels(cluster));
</script>

<div class="card-row">
  <label class="card__check">
    <input
      type="checkbox"
      {checked}
      data-testid="cluster-select-checkbox"
      data-cluster-id={cluster.clusterId}
      aria-label={`그룹 ${cluster.clusterId}을(를) 일괄 삭제 대상으로 선택`}
      onclick={(e) => e.stopPropagation()}
      onchange={() => onchecktoggle?.(cluster)}
    />
  </label>
  <button
    class="card"
    class:card--selected={selected}
    type="button"
    aria-pressed={selected}
    data-cluster-id={cluster.clusterId}
    onclick={() => onselect?.(cluster)}
  >
  <div class="card__thumbs">
    {#each thumbs as member (member.fileId)}
      <Thumbnail
        src={member.thumbnailUrl}
        fileId={member.fileId}
        {fetchThumbnail}
        alt={`그룹 ${cluster.clusterId} · 파일 ${member.fileId}`}
      />
    {/each}
    {#if overflow > 0}
      <span class="card__more">+{overflow}</span>
    {/if}
  </div>

  <div class="card__meta">
    <div class="card__top">
      <span class="card__badges">
        {#each badges as trust (trust)}
          <TrustBadge {trust} collapseAt={470} />
        {/each}
        {#if cluster.introOutro}
          <span class="card__introoutro" data-testid="cluster-intro-outro-badge"
            >인트로/아웃트로 의심</span
          >
        {/if}
      </span>
      <span class="card__count">{cluster.members.length}개 파일</span>
    </div>
    <div class="card__title">
      {best ? `${resolutionLabel(best.width, best.height)} · 최적 사본 기준` : "파일 정보 갱신 중"}
    </div>
    <div class="card__sub">정리 시 회수 {formatBytes(reclaimable)}</div>
  </div>

  <span class="card__chevron" aria-hidden="true">›</span>
  </button>
</div>

<style>
  .card-row {
    display: flex;
    align-items: center;
    gap: var(--space-xxs);
    width: 100%;
    height: 100%;
  }
  .card__check {
    flex: 0 0 auto;
    display: flex;
    align-items: center;
    padding: var(--space-xxs);
    cursor: pointer;
  }
  .card {
    display: flex;
    align-items: center;
    gap: var(--space-sm);
    flex: 1 1 auto;
    min-width: 0;
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
  



  @container (max-width: 470px) {
    .card__top {
      flex-wrap: wrap;
    }
  }
  

  .card__badges {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-xxxs);
    min-width: 0;
  }
  .card__count {
    color: var(--color-body);
    font-size: var(--type-body-sm-size);
    white-space: nowrap;
  }
  





  .card__introoutro {
    padding: 0 var(--space-xxs);
    color: var(--color-semantic-warning);
    border: 1px solid var(--color-semantic-warning);
    font-size: var(--type-caption-size);
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
