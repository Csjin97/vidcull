<script lang="ts">
  import DeleteDialog from "./DeleteDialog.svelte";
  import Icon from "./Icon.svelte";
  import OverlapTimeline from "./OverlapTimeline.svelte";
  import Thumbnail from "./Thumbnail.svelte";
  import TrustBadge from "./TrustBadge.svelte";
  import { membersByQuality, resolveBestFileId } from "$lib/model/best-copy";
  import {
    fileName,
    formatBitrate,
    formatBytes,
    formatDuration,
    formatResolution,
    parentDir,
  } from "$lib/model/format";
  import {
    assessClusterDeletion,
    assessDeletion,
    defaultClusterSelection,
    defaultSelection,
    type DeleteMode,
  } from "$lib/model/safe-delete";
  import { rejectMessage } from "$lib/model/delete-messages";
  import { partitionByIntroOutro } from "$lib/model/timeline";
  import { resolveTimelineMode } from "$lib/model/timeline-visibility";
  import { introOutroVisibilityStore } from "$lib/stores/introOutroVisibilityStore.svelte";
  import { normalizeError } from "$lib/errors/normalizeError";
  import { untrack, onMount } from "svelte";
  import { revealInFolder } from "$lib/ipc/tauri";
  import { loadSettings, type Settings } from "$lib/data/settings";
  import type { DeleteOutcome } from "$lib/data/datasource";
  import type {
    ClipOverlap,
    ContentCluster,
    DuplicateGroup,
  } from "$lib/model/types";

  let {
    group,
    cluster = null,
    overlaps = [],
    onclose,
    onexecute,
    fetchThumbnail,
  }: {
    group: DuplicateGroup;
    cluster?: ContentCluster | null;
    overlaps?: ClipOverlap[];
    onclose: () => void;
    onexecute: (
      fileIds: number[],
      mode: DeleteMode,
    ) => Promise<DeleteOutcome>;
    fetchThumbnail?: (fileId: number) => Promise<string | null>;
  } = $props();


  function initialSelection(): Set<number> {
    return cluster ? defaultClusterSelection(cluster) : defaultSelection(group);
  }

  const lengthRatio = $derived.by(() => {
    const durations = group.members.map((m) => m.durationMs);
    if (durations.length < 2) return 1;
    const max = Math.max(...durations);
    if (max <= 0) return 1;
    return Math.min(...durations) / max;
  });

  const introOutroPartition = $derived(partitionByIntroOutro(overlaps));
  const effectiveOverlaps = $derived(
    introOutroVisibilityStore.shown ? overlaps : introOutroPartition.shown,
  );

  const timelineMode = $derived(
    resolveTimelineMode(group.trust, effectiveOverlaps.length > 0, lengthRatio),
  );

  const overlapSource = $derived.by(() => {
    switch (timelineMode.kind) {
      case "hidden":
        return null;
      case "full-span":
        return group.members[0] ?? null;
      case "partial": {
        const canonical = group.members.find(
          (m) => m.fileId === effectiveOverlaps[0].sourceFileId,
        );
        if (canonical) return canonical;
        for (const o of effectiveOverlaps) {
          const survivor = group.members.find(
            (m) => m.fileId === o.sourceFileId || m.fileId === o.clipFileId,
          );
          if (survivor) return survivor;
        }
        return null;
      }
    }
  });

  let selectedMemberId = $state<number | null>(null);

  let perspectiveSourceId = untrack(() => overlapSource?.fileId ?? null);
  $effect(() => {
    const sourceId = overlapSource?.fileId ?? null;
    if (sourceId !== perspectiveSourceId) {
      perspectiveSourceId = sourceId;
      selectedMemberId = sourceId;
    } else if (selectedMemberId === null && sourceId !== null) {
      selectedMemberId = sourceId;
    }
  });

  const perspective = $derived.by(() => {
    const id = selectedMemberId;
    if (id === null) return null;
    const member = group.members.find((m) => m.fileId === id) ?? null;
    if (!member) return null;

    if (timelineMode.kind === "hidden") return null;

    if (timelineMode.kind === "full-span") {
      const synthetic: ClipOverlap[] = group.members
        .filter((m) => m.fileId !== id)
        .map((other) => ({
          clipFileId: other.fileId,
          sourceFileId: id,
          matchedScenes: 1,
          clipScenes: 1,
          startMs: 0,
          endMs: member.durationMs,
          clipStartMs: 0,
          clipEndMs: other.durationMs,
        }));
      if (synthetic.length === 0) {
        synthetic.push({
          clipFileId: id,
          sourceFileId: id,
          matchedScenes: 1,
          clipScenes: 1,
          startMs: 0,
          endMs: member.durationMs,
          clipStartMs: 0,
          clipEndMs: member.durationMs,
        });
      }
      return {
        durationMs: member.durationMs,
        label: fileName(member.path),
        overlaps: synthetic,
      };
    }

    const remapped: ClipOverlap[] = [];
    for (const o of effectiveOverlaps) {
      if (o.sourceFileId === id) {
        remapped.push({ ...o, clipFileId: o.clipFileId });
      } else if (o.clipFileId === id) {
        remapped.push({
          ...o,
          clipFileId: o.sourceFileId,
          startMs: o.clipStartMs,
          endMs: o.clipEndMs,
        });
      }
    }
    if (remapped.length === 0) return null;
    return {
      durationMs: member.durationMs,
      label: fileName(member.path),
      overlaps: remapped,
    };
  });

  const headingLabel = $derived(
    timelineMode.kind === "full-span" ? "전체 일치" : "부분 클립 겹침",
  );
  const timelineNotice = $derived.by(() => {
    if (timelineMode.kind !== "full-span") return undefined;
    return timelineMode.notice === "whole-file-est"
      ? "전체 구간 유사 (추정 · 겹침 데이터 없음)"
      : "겹침 구간 데이터 없음 — 전체 구간 임시 표시";
  });

  let selected = $state(untrack(() => initialSelection()));
  let mode = $state<DeleteMode>("trash");
  let dialogOpen = $state(false);
  let busy = $state(false);
  let revealError = $state<string | null>(null);
  let deleteError = $state<string | null>(null);

  let openGroupId = untrack(() => group.groupId);
  $effect(() => {
    if (group.groupId !== openGroupId) {
      openGroupId = group.groupId;
      selected = initialSelection();
      mode = "trash";
      revealError = null;
      deleteError = null;
    }
  });

  let settings = $state<Settings | null>(null);

  onMount(async () => {
    try {
      settings = await loadSettings();
    } catch (err) {
      console.error("Failed to load settings in CompareView:", err);
    }
  });

  const activeMode = $derived(settings?.bestCopyMode ?? "archival");
  const ordered = $derived(membersByQuality(group, activeMode));
  const bestId = $derived(resolveBestFileId(group, activeMode, activeMode));

  const modeBestIds = $derived({
    archival: resolveBestFileId(group, "archival", activeMode),
    space_saving: resolveBestFileId(group, "space_saving", activeMode),
    max_quality: resolveBestFileId(group, "max_quality", activeMode),
    min_size: resolveBestFileId(group, "min_size", activeMode),
    compatible: resolveBestFileId(group, "compatible", activeMode),
    max_resolution: resolveBestFileId(group, "max_resolution", activeMode),
  });

  function getAltBadges(fileId: number): string[] {
    const badges: string[] = [];
    if (fileId === bestId) return badges;

    if (activeMode !== "space_saving" && fileId === modeBestIds.space_saving) {
      badges.push("고효율 최적");
    }
    if (activeMode !== "max_quality" && fileId === modeBestIds.max_quality) {
      badges.push("최고 화질");
    }
    if (activeMode !== "min_size" && fileId === modeBestIds.min_size) {
      badges.push("최소 용량");
    }
    if (activeMode !== "archival" && fileId === modeBestIds.archival) {
      badges.push("원본 예상");
    }
    if (activeMode !== "compatible" && fileId === modeBestIds.compatible) {
      badges.push("범용 호환");
    }
    if (activeMode !== "max_resolution" && fileId === modeBestIds.max_resolution) {
      badges.push("최고 해상도");
    }
    return badges;
  }
  const assessment = $derived(
    cluster
      ? assessClusterDeletion(cluster, selected, mode)
      : assessDeletion(group, selected, mode),
  );


  function selectMember(fileId: number): void {
    selectedMemberId = fileId;
  }

  function toggle(fileId: number): void {
    const next = new Set(selected);
    if (next.has(fileId)) {
      next.delete(fileId);
    } else {
      next.add(fileId);
    }
    selected = next;
  }

  async function reveal(path: string): Promise<void> {
    revealError = null;
    try {
      await revealInFolder(path);
    } catch (err) {
      revealError = String(err);
    }
  }

  async function confirmDelete(): Promise<void> {
    if (!assessment.canProceed) return;
    deleteError = null;
    busy = true;
    try {
      const outcome = await onexecute([...selected], mode);
      if (outcome.ok) {
        dialogOpen = false;
      } else if (outcome.rejectCode !== null) {
        deleteError = normalizeError(outcome.detail || "삭제에 실패했습니다. 다시 시도해 주세요.").message;
      }
    } finally {
      busy = false;
    }
  }
</script>

<section class="compare">
  <header class="compare__head">
    <div class="compare__heading">
      <TrustBadge trust={group.trust} collapseAt={428} />
      <h2 class="compare__title">그룹 #{group.groupId}</h2>
      <span class="compare__count">{group.members.length}개 사본</span>
    </div>
    <button class="btn btn--ghost" type="button" onclick={onclose}>닫기 ✕</button>
  </header>

  <div class="compare__scroll">
  {#if introOutroPartition.hidden.length > 0}
    <div class="intro-outro-bar">
      <label class="intro-outro-bar__toggle">
        <input
          type="checkbox"
          checked={introOutroVisibilityStore.shown}
          onchange={() => introOutroVisibilityStore.toggle()}
        />
        인트로/아웃트로 겹침 표시
      </label>
      {#if !introOutroVisibilityStore.shown}
        <span class="intro-outro-bar__count" data-testid="intro-outro-hidden-count">
          인트로/아웃트로 겹침 {introOutroPartition.hidden.length}건 숨김
        </span>
      {/if}
    </div>
  {/if}

  {#if perspective}
    <OverlapTimeline
      sourceDurationMs={perspective.durationMs}
      overlaps={perspective.overlaps}
      sourceLabel={perspective.label}
      {headingLabel}
      notice={timelineNotice}
    />
  {/if}

  <div class="compare__grid">
    {#each ordered as member (member.fileId)}
      {@const isBest = member.fileId === bestId}
      {@const altBadges = getAltBadges(member.fileId)}
      {@const isFocused = member.fileId === selectedMemberId}
      <article
        class="member"
        class:member--best={isBest}
        class:member--focused={isFocused}
      >
        
        <button
          type="button"
          class="member__focus"
          aria-pressed={isFocused}
          aria-label={`${fileName(member.path)} 기준으로 겹침 구간 보기`}
          title="이 영상 기준으로 겹침 구간 보기"
          onclick={() => selectMember(member.fileId)}
        >
        <div class="member__thumb">
          <Thumbnail
            src={member.thumbnailUrl}
            fileId={member.fileId}
            {fetchThumbnail}
            alt={`파일 ${member.fileId} 미리보기`}
          />
          {#if isBest}
            <span class="member__bestflag">최적 사본</span>
          {:else if altBadges.length > 0}
            <div class="member__altbadges">
              {#each altBadges as badge}
                <span class="member__altbestflag">{badge}</span>
              {/each}
            </div>
          {/if}
        </div>

        <h3 class="member__name" title={member.path}>{fileName(member.path)}</h3>
        <p class="member__path" title={member.path}>{parentDir(member.path)}</p>

        <dl class="member__specs">
          <div><dt>해상도</dt><dd>{formatResolution(member.width, member.height)}</dd></div>
          <div><dt>길이</dt><dd>{formatDuration(member.durationMs)}</dd></div>
          <div><dt>크기</dt><dd>{formatBytes(member.sizeBytes)}</dd></div>
          <div><dt>비트레이트</dt><dd>{formatBitrate(member.bitrateBps)}</dd></div>
          <div><dt>코덱</dt><dd>{member.codec} · {member.container}</dd></div>
        </dl>
        </button>

        <div class="member__actions">
          <label class="member__select">
            <input
              type="checkbox"
              checked={selected.has(member.fileId)}
              onchange={() => toggle(member.fileId)}
            />
            삭제 선택
          </label>
          <button
            class="btn btn--ghost"
            type="button"
            title="폴더 열기"
            aria-label="폴더 열기"
            onclick={() => reveal(member.path)}
          >
            <Icon name="folder" class="cmp__icon" />
            <span class="cmp__label">폴더 열기</span>
          </button>
        </div>
      </article>
    {/each}
  </div>

  {#if revealError}
    <p class="compare__notice">{revealError}</p>
  {/if}
  </div>

  <footer class="compare__foot">
    <div class="compare__modes" role="radiogroup" aria-label="삭제 방식">
      <label>
        <input type="radio" name="mode" value="trash" bind:group={mode} />
        휴지통으로 이동 (기본)
      </label>
      <label>
        <input type="radio" name="mode" value="permanent" bind:group={mode} />
        영구 삭제
      </label>
    </div>

    <div class="compare__summary">
      {#if assessment.plan}
        <span>{assessment.plan.toDelete.length}개 선택 · 회수 {formatBytes(assessment.plan.reclaimedBytes)}</span>
      {:else}
        <span class="compare__blocked">
          {assessment.issues.find((i) => i.level === "error")?.message ?? rejectMessage("NONE_SELECTED")}
        </span>
      {/if}
      <button
        class="btn {mode === 'permanent' ? 'btn--danger' : 'btn--primary'}"
        type="button"
        disabled={!assessment.canProceed}
        title={mode === "permanent" ? "영구 삭제" : "휴지통으로 이동"}
        aria-label={mode === "permanent" ? "영구 삭제" : "휴지통으로 이동"}
        onclick={() => (dialogOpen = true)}
      >
        <Icon name="trash" class="cmp__icon" />
        <span class="cmp__label">{mode === "permanent" ? "영구 삭제" : "휴지통으로 이동"}</span>
      </button>
    </div>
  </footer>

  {#if deleteError}
    <p class="compare__notice compare__notice--error" data-testid="delete-error">{deleteError}</p>
  {/if}

  <DeleteDialog
    bind:open={dialogOpen}
    {assessment}
    {mode}
    {busy}
    onconfirm={confirmDelete}
  />
</section>

<style>
  .compare {
    display: flex;
    flex-direction: column;
    gap: var(--space-sm);
    height: 100%;
    padding: var(--space-sm);
    background: var(--color-canvas);
    border-left: 1px solid var(--color-hairline);
    


    overflow: hidden;
    

    container-type: inline-size;
  }
  .compare__scroll {
    flex: 1 1 auto;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: var(--space-sm);
  }
  

  .intro-outro-bar {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-sm);
    font-size: var(--type-body-sm-size);
    color: var(--color-body);
  }
  .intro-outro-bar__toggle {
    display: flex;
    align-items: center;
    gap: var(--space-xxxs);
    cursor: pointer;
  }
  .intro-outro-bar__count {
    color: var(--color-muted);
  }
  
  :global(.cmp__icon) {
    display: none;
    font-size: 1.05rem;
  }
  @container (max-width: 380px) {
    .cmp__label {
      display: none;
    }
    :global(.cmp__icon) {
      display: inline-flex;
    }
  }
  .compare__head {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .compare__heading {
    display: flex;
    align-items: center;
    gap: var(--space-xs);
  }
  .compare__title {
    font-size: var(--type-display-md-size);
  }
  .compare__count {
    color: var(--color-body);
    font-size: var(--type-body-sm-size);
  }
  .compare__grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: var(--space-sm);
  }
  .member {
    display: flex;
    flex-direction: column;
    gap: var(--space-xxs);
    padding: var(--space-xs);
    background: var(--color-surface-card);
    border: 1px solid var(--color-hairline);
  }
  .member--best {
    border-color: var(--color-primary);
  }
  

  .member--focused {
    border-color: var(--color-semantic-warning);
    box-shadow: inset 0 0 0 1px var(--color-semantic-warning);
  }
  

  .member__focus {
    display: flex;
    flex-direction: column;
    gap: var(--space-xxs);
    flex: 1 1 auto;
    appearance: none;
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    font: inherit;
    color: inherit;
    text-align: left;
    cursor: pointer;
  }
  .member__focus:focus-visible {
    outline: 2px solid var(--color-semantic-warning);
    outline-offset: 2px;
  }
  .member__thumb {
    position: relative;
  }
  .member__bestflag {
    position: absolute;
    top: var(--space-xxxs);
    left: var(--space-xxxs);
    padding: 0 var(--space-xxs);
    background: var(--color-primary);
    color: var(--color-on-primary);
    font-size: var(--type-caption-uppercase-size);
    font-weight: var(--type-caption-uppercase-weight);
    letter-spacing: var(--type-caption-uppercase-tracking);
    text-transform: var(--type-caption-uppercase-transform);
  }
  .member__altbadges {
    position: absolute;
    top: var(--space-xxxs);
    left: var(--space-xxxs);
    display: flex;
    flex-direction: column;
    gap: var(--space-xxxs);
  }
  .member__altbestflag {
    padding: 0 var(--space-xxs);
    background: rgba(0, 0, 0, 0.7);
    color: #ffffff;
    border: 1px solid rgba(255, 255, 255, 0.3);
    font-size: var(--type-caption-uppercase-size);
    font-weight: var(--type-caption-uppercase-weight);
    letter-spacing: var(--type-caption-uppercase-tracking);
    text-transform: var(--type-caption-uppercase-transform);
    white-space: nowrap;
  }
  .member__name {
    font-size: var(--type-title-sm-size);
    font-weight: var(--font-weight-display);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .member__path {
    margin: 0;
    color: var(--color-muted);
    font-size: var(--type-caption-size);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .member__specs {
    display: flex;
    flex-direction: column;
    gap: var(--space-xxxs);
    margin: 0;
  }
  .member__specs div {
    display: flex;
    justify-content: space-between;
    font-size: var(--type-body-sm-size);
  }
  .member__specs dt {
    color: var(--color-muted);
  }
  .member__specs dd {
    margin: 0;
    color: var(--color-ink);
  }
  .member__actions {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-top: auto;
    padding-top: var(--space-xxs);
  }
  .member__select {
    display: flex;
    align-items: center;
    gap: var(--space-xxxs);
    font-size: var(--type-body-sm-size);
  }
  .compare__notice {
    margin: 0;
    color: var(--color-semantic-warning);
    font-size: var(--type-body-sm-size);
  }
  .compare__notice--error {
    color: var(--color-semantic-error, #c0392b);
  }
  .compare__foot {
    



    display: flex;
    flex-direction: column;
    align-items: stretch;
    gap: var(--space-xs);
    flex: none;
    padding: var(--space-xs) 0 var(--space-xxs);
    border-top: 1px solid var(--color-hairline);
    background: var(--color-canvas);
  }
  .compare__modes {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-sm);
    font-size: var(--type-body-sm-size);
  }
  .compare__modes label {
    display: flex;
    align-items: center;
    gap: var(--space-xxxs);
  }
  .compare__summary {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-sm);
    font-size: var(--type-body-sm-size);
  }
  .compare__blocked {
    color: var(--color-semantic-warning);
  }
</style>
