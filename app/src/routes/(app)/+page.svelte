<!--
  [변경 이력 (Changelog)]
  - 2026-08-03 : 중첩 갱신 방지, 화면 초기화 및 클러스터 조회 성능 개선
-->
<script lang="ts">
  import { onMount } from "svelte";
  import CompareView from "$lib/components/CompareView.svelte";
  import ClusterCard from "$lib/components/ClusterCard.svelte";
  import BulkDeleteDialog from "$lib/components/BulkDeleteDialog.svelte";
  import FailureAccordion from "$lib/components/FailureAccordion.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import ProgressPanel from "$lib/components/ProgressPanel.svelte";
  import VirtualList from "$lib/components/VirtualList.svelte";
  import { dataSource } from "$lib/app-data";
  import { frontendTraceEnabled, ipcTraceBuffer, pickFolder } from "$lib/ipc/tauri";
  import { loadSettings, saveSettings, setIndexingEnabled } from "$lib/data/settings";
  import { resolveBestFileId } from "$lib/model/best-copy";
  import { formatBytes } from "$lib/model/format";
  import {
    clusterAsGroup,
    clusterOverlapGroupIds,
    clustersEqual,
    partitionClustersByIntroOutro,
    shouldShowIntroOutroBar,
  } from "$lib/model/cluster";
  import { introOutroVisibilityStore } from "$lib/stores/introOutroVisibilityStore.svelte";
  import type { DeleteOutcome, UndoOutcome } from "$lib/data/datasource";
  import {
    EtaEstimator,
    ProgressHistory,
    drainBytesPerSec,
    formatRelativeActivity,
    isDrainStalled,
    errorBackoffMs,
    isFolderScanning,
    isScanning,
    nextActivityMs,
    pollIntervalMs,
    refreshLimit,
    remaining,
    shouldRefreshGroups,
  } from "$lib/model/progress";
  import {
    FrameTimingBuffer,
    sampleFrames,
    type FrameTimingStats,
  } from "$lib/model/frame-timing";
  import {
    planBulkDeletion,
    type BulkDeletionPlan,
    type DeleteMode,
  } from "$lib/model/safe-delete";
  import type {
    ClipOverlap,
    ContentCluster,
    FailedTask,
    ProgressSnapshot,
    TrustLevel,
  } from "$lib/model/types";
  import { collapseOnScroll } from "$lib/model/progress";
  import { errorStore } from "$lib/stores/errorStore.svelte";

  const PAGE_SIZE = 30;
  const ROW_HEIGHT = 140;

  const PROGRESS_WINDOW = 120;

  const FAILED_TASKS_LIMIT = 100;

  type Filter = TrustLevel | "ALL";

  let filter = $state<Filter>("ALL");
  let clusters = $state<ContentCluster[]>([]);
  let total = $state(0);
  let reclaimable = $state(0);
  let loading = $state(false);

  const introOutroSplit = $derived(partitionClustersByIntroOutro(clusters));
  const visibleClusters = $derived(
    introOutroVisibilityStore.shown ? clusters : introOutroSplit.visible,
  );
  const hiddenIntroOutroCount = $derived(introOutroSplit.hidden.length);
  let selectedCluster = $state<ContentCluster | null>(null);
  let selectedOverlaps = $state<ClipOverlap[]>([]);
  let progressCollapsed = $state(false);

  let listWidth = $state<number | null>(null);
  let reviewEl: HTMLDivElement | undefined = $state();

  function startResize(event: PointerEvent): void {
    event.preventDefault();
    const container = reviewEl;
    if (!container) return;
    const onMove = (e: PointerEvent) => {
      const rect = container.getBoundingClientRect();
      const next = Math.min(
        Math.max(e.clientX - rect.left, 300),
        rect.width - 390,
      );
      listWidth = next;
    };
    const onUp = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  }

  function onListScroll(scrollTop: number): void {
    progressCollapsed = collapseOnScroll(scrollTop, progressCollapsed);
  }

  const history = new ProgressHistory(PROGRESS_WINDOW);
  const etaEstimator = new EtaEstimator();
  let prevRemainingForEta = 0;
  let progressSnapshot = $state<ProgressSnapshot | null>(null);
  let lastGroupsRevision: number | null = null;
  let throughput = $state<number[]>([]);
  let eta = $state<number | null>(null);
  let speedBytesPerSec = $state(0);
  let speedKnown = $state(false);
  let stalled = $state(false);
  let failedTasks = $state<FailedTask[]>([]);

  let scanFolderCount = $state(0);

  let indexingEnabled = $state(true);
  let lastIndexActivityMs = $state<number | null>(null);
  let prevDone = $state<number | null>(null);
  let nowMs = $state(Date.now());

  let undoBatchCount = $state(0);
  let undoBannerDetail = $state("");
  let undoBannerVisible = $state(false);
  let undoBannerOk = $state(true);
  let undoInProgress = $state(false);

  let bulkSelectedIds = $state<Set<number>>(new Set());
  let bulkDialogOpen = $state(false);
  let bulkPlan = $state<BulkDeletionPlan | null>(null);
  let bulkMode = $state<DeleteMode>("trash");
  let bulkBusy = $state(false);
  const EMPTY_BULK_PLAN: BulkDeletionPlan = {
    clusterCount: 0,
    skippedClusterIds: [],
    toDelete: [],
    kept: [],
    reclaimedBytes: 0,
    perCluster: [],
  };

  let failuresOpen = $state(false);
  const failedCount = $derived(
    Math.max(progressSnapshot?.failed ?? 0, failedTasks.length),
  );

  const lastActivityLabel = $derived(formatRelativeActivity(lastIndexActivityMs, nowMs));

  let consecutiveErrors = 0;
  let reloadInFlight: Promise<void> | null = null;
  let selectionRequestId = 0;

  const frameTimer = new FrameTimingBuffer(PROGRESS_WINDOW);
  let stopFrameSampling: (() => void) | null = null;

  const FRAME_JANK_REPORT_THRESHOLD = 0.1;

  function reportFrameTimingIfJanky(scanning: boolean): void {
    if (!scanning) {
      frameTimer.clear();
      return;
    }
    const stats: FrameTimingStats = frameTimer.stats();
    if (stats.frames > 0 && stats.droppedFraction >= FRAME_JANK_REPORT_THRESHOLD) {
      const ipcSummary = ipcTraceBuffer.summary();
      console.warn(
        `render jank during scan: ${stats.droppedFrames}/${stats.frames} frames over budget ` +
          `(p95=${stats.p95Ms.toFixed(1)}ms max=${stats.maxMs.toFixed(1)}ms mean=${stats.meanMs.toFixed(1)}ms)` +
          (ipcSummary === "" ? "" : ` ipc[${ipcSummary}]`),
      );
    }
    frameTimer.clear();
  }

  async function toggleIndexing(): Promise<void> {
    const next = !indexingEnabled;
    indexingEnabled = next; 
    try {
      const saved = await setIndexingEnabled(next);
      indexingEnabled = saved.indexingEnabled;
    } catch {
      indexingEnabled = !next; 
    }
  }

  async function pollProgress(): Promise<void> {
    let anySuccess = false;
    try {
      const snapshot = await dataSource.progress();
      nowMs = Date.now();
      history.push({ timestampMs: nowMs, snapshot });
      progressSnapshot = snapshot;
      lastIndexActivityMs = nextActivityMs(prevDone, snapshot.done, lastIndexActivityMs, nowMs);
      prevDone = snapshot.done;
      const samples = history.samples();
      throughput = history.bytesThroughput();
      const rate = drainBytesPerSec(samples);
      speedBytesPerSec = rate ?? 0;
      speedKnown = rate !== null;
      const remNow = remaining(snapshot);
      if (prevRemainingForEta === 0 && remNow > 0) {
        etaEstimator.reset();
      }
      const etaPaused = !indexingEnabled && isScanning(snapshot);
      etaEstimator.push({ timestampMs: nowMs, snapshot }, { paused: etaPaused });
      prevRemainingForEta = remNow;
      eta = etaEstimator.displayEta();
      stalled = isDrainStalled(samples);
      anySuccess = true;
      if (!loadedOk && !loading) {
        void reload();
      } else if (loadedOk && !loading) {
        void refreshGroupList();
      }

    } catch (err) {
      console.warn("progress poll failed (will retry):", err);
    }
    try {
      failedTasks = await dataSource.failedTasks(FAILED_TASKS_LIMIT);
      anySuccess = true;
    } catch (err) {
      console.warn("failed-tasks poll failed (will retry):", err);
    }

    if (anySuccess) {
      consecutiveErrors = 0;
    } else {
      consecutiveErrors += 1;
    }

    reportFrameTimingIfJanky(
      progressSnapshot ? isScanning(progressSnapshot) : false,
    );
  }

  let pollTimer: ReturnType<typeof setTimeout> | null = null;

  function scheduleNextPoll(): void {
    let delayMs: number;
    if (consecutiveErrors > 0) {
      delayMs = errorBackoffMs(consecutiveErrors);
    } else {
      const scanning = progressSnapshot
        ? isScanning(progressSnapshot) || isFolderScanning(progressSnapshot)
        : true;
      delayMs = pollIntervalMs(scanning);
    }
    pollTimer = setTimeout(async () => {
      await pollProgress();
      scheduleNextPoll();
    }, delayMs);
  }

  async function selectCluster(cluster: ContentCluster): Promise<void> {
    const requestId = ++selectionRequestId;
    selectedCluster = cluster;
    selectedOverlaps = [];
    try {
      const overlaps = (
        await Promise.all(
          clusterOverlapGroupIds(cluster).map((g) => dataSource.partialOverlaps(g)),
        )
      ).flat();
      if (requestId === selectionRequestId) selectedOverlaps = overlaps;
    } catch (err) {
      if (requestId === selectionRequestId) errorStore.reportError(err);
    }
  }

  const tabs: Array<{ id: Filter; label: string; shortLabel?: string; icon: string }> = [
    { id: "ALL", label: "전체", icon: "all" },
    { id: "EXACT", label: "완전 동일", icon: "exact" },
    { id: "VERY_LIKELY", label: "유사 (재인코딩)", shortLabel: "재인코딩", icon: "reencode" },
    { id: "POSSIBLE", label: "유사 (추정)", shortLabel: "추정", icon: "possible" },
  ];

  function trustArg(): TrustLevel | undefined {
    return filter === "ALL" ? undefined : filter;
  }

  function loadThumbnail(fileId: number): Promise<string | null> {
    return dataSource.fetchThumbnail(fileId);
  }

  let loadedOk = $state(false);

  let addingFolder = $state(false);
  let addFolderError = $state("");

  async function addScanFolder(): Promise<void> {
    addingFolder = true;
    addFolderError = "";
    try {
      const picked = await pickFolder();
      if (!picked) return; 
      const normalized = picked.replace(/\\/g, "/");
      const settings = await loadSettings();
      if (!settings.scanFolders.includes(normalized)) {
        const next = { ...settings, scanFolders: [...settings.scanFolders, normalized] };
        await saveSettings(next);
        scanFolderCount = next.scanFolders.length;
      } else {
        scanFolderCount = settings.scanFolders.length;
      }
      await reload();
    } catch (err) {
      addFolderError = `폴더를 추가하지 못했습니다: ${String(err)}`;
    } finally {
      addingFolder = false;
    }
  }

  async function reload(): Promise<void> {
    if (reloadInFlight) return reloadInFlight;
    reloadInFlight = performReload();
    try {
      await reloadInFlight;
    } finally {
      reloadInFlight = null;
    }
  }

  async function performReload(): Promise<void> {
    loading = true;
    try {
      selectionRequestId += 1;
      selectedCluster = null;
      selectedOverlaps = [];
      total = await dataSource.countClusters(trustArg());
      reclaimable = await dataSource.clusterReclaimableBytes();
      clusters = await dataSource.listClusters({
        trust: trustArg(),
        limit: PAGE_SIZE,
        offset: 0,
      });
      loadedOk = true;
      lastGroupsRevision = progressSnapshot?.groupsRevision ?? null;
    } catch (err) {
      console.warn("cluster reload failed (will retry):", err);
      loadedOk = false;
    } finally {
      loading = false;
    }
  }

  async function refreshGroupList(): Promise<void> {
    if (loading || reloadInFlight) return;

    // 데몬이 보고하는 groups_revision이 안 바뀌었으면 그룹 구성 자체가 바뀌지
    // 않았다는 뜻이다 — count/reclaimable/list IPC 왕복 전부를 건너뛴다. 구버전
    // 데몬처럼 이 필드가 없으면(null) 아래 total 기반 경로로 그대로 폴백한다.
    const groupsRevision = progressSnapshot?.groupsRevision ?? null;
    if (
      groupsRevision !== null &&
      lastGroupsRevision !== null &&
      groupsRevision === lastGroupsRevision
    ) {
      return;
    }

    let prevTotal: number;
    let newTotal: number;
    let scanning: boolean;
    try {
      prevTotal = total;
      newTotal = await dataSource.countClusters(trustArg());
      total = newTotal;
      scanning = progressSnapshot ? isScanning(progressSnapshot) : false;
      if (!shouldRefreshGroups(prevTotal, newTotal, scanning, selectedCluster !== null)) {
        lastGroupsRevision = groupsRevision;
        return;
      }
    } catch (err) {
      console.warn("group list refresh failed (will retry):", err);
      return;
    }

    loading = true;
    const refreshStartedAt = frontendTraceEnabled ? performance.now() : null;
    try {
      reclaimable = await dataSource.clusterReclaimableBytes();
      const next = await dataSource.listClusters({
        trust: trustArg(),
        limit: refreshLimit(PAGE_SIZE, clusters.length),
        offset: 0,
      });
      if (!clustersEqual(clusters, next)) {
        clusters = next;
      }
      loadedOk = true;
      lastGroupsRevision = groupsRevision;
    } catch (err) {
      console.warn("group list refresh failed (will retry):", err);
    } finally {
      loading = false;
      if (refreshStartedAt !== null) {
        console.debug(
          `[ipc-trace] refreshGroupList await span ${(performance.now() - refreshStartedAt).toFixed(1)}ms`,
        );
      }
    }
  }

  async function loadMore(): Promise<void> {
    if (loading || clusters.length >= total) return;
    loading = true;
    try {
      const next = await dataSource.listClusters({
        trust: trustArg(),
        limit: PAGE_SIZE,
        offset: clusters.length,
      });
      clusters = [...clusters, ...next];
    } catch (err) {
      console.warn("loadMore failed (will retry on next scroll):", err);
    } finally {
      loading = false;
    }
  }

  async function setFilter(next: Filter): Promise<void> {
    if (next === filter) return;
    filter = next;
    await reload();
  }

  async function resetUi(): Promise<void> {
    selectionRequestId += 1;
    selectedCluster = null;
    selectedOverlaps = [];
    bulkSelectedIds = new Set();
    bulkDialogOpen = false;
    bulkPlan = null;
    failuresOpen = false;
    progressCollapsed = false;
    listWidth = null;
    undoBannerVisible = false;
    errorStore.dismissAll();
    history.clear();
    etaEstimator.reset();
    throughput = [];
    eta = null;
    stalled = false;
    await reload();
  }

  interface ClusterDeleteResult {
    ok: boolean;
    removedFileIds: number[];
    reclaimedBytes: number;
    detail: string;
    rejectCode: string | null;
    batches: number;
  }

  // Fans one cluster's selected file ids out into one deleteFiles IPC call
  // per underlying duplicate group (a cluster can span several groups).
  // Shared by both the single-cluster review flow (execute) and bulk cleanup
  // (confirmBulkDelete), which just calls this once per selected cluster.
  async function deleteClusterFiles(
    cluster: ContentCluster,
    fileIds: readonly number[],
    mode: DeleteMode,
  ): Promise<ClusterDeleteResult> {
    const best = resolveBestFileId(clusterAsGroup(cluster));
    const byGroup = new Map<number, number[]>();
    for (const fileId of fileIds) {
      const groupId = cluster.members.find((m) => m.fileId === fileId)?.groupId;
      if (groupId === undefined) continue;
      byGroup.set(groupId, [...(byGroup.get(groupId) ?? []), fileId]);
    }

    let ok = byGroup.size > 0;
    const removedFileIds: number[] = [];
    let reclaimedBytes = 0;
    const details: string[] = [];
    let rejectCode: string | null = null;

    for (const [groupId, ids] of byGroup) {
      const confirmBest = best !== null && ids.includes(best);
      const outcome = await dataSource.deleteFiles(groupId, ids, mode, confirmBest);
      ok = ok && outcome.ok;
      removedFileIds.push(...outcome.removedFileIds);
      reclaimedBytes += outcome.reclaimedBytes;
      details.push(outcome.detail);
      if (!outcome.ok && outcome.rejectCode !== null && rejectCode === null) {
        rejectCode = outcome.rejectCode;
      }
    }

    return {
      ok,
      removedFileIds,
      reclaimedBytes,
      detail: details.join(" "),
      rejectCode,
      batches: byGroup.size,
    };
  }

  async function execute(
    fileIds: number[],
    mode: DeleteMode,
  ): Promise<DeleteOutcome> {
    const target = selectedCluster;
    if (!target) {
      return {
        ok: false,
        removedFileIds: [],
        reclaimedBytes: 0,
        detail: "선택된 그룹이 없습니다.",
        rejectCode: null,
      };
    }

    let result: ClusterDeleteResult;
    try {
      result = await deleteClusterFiles(target, fileIds, mode);
    } catch (err) {
      errorStore.reportError(err);
      return {
        ok: false,
        removedFileIds: [],
        reclaimedBytes: 0,
        detail: "삭제 중 오류가 발생했습니다.",
        rejectCode: null,
      };
    }

    if (result.removedFileIds.length > 0) {
      undoBatchCount = result.batches;
      undoBannerDetail = result.detail || "삭제되었습니다.";
      undoBannerOk = true;
      undoBannerVisible = true;

      try {
        reclaimable = await dataSource.clusterReclaimableBytes();
        total = await dataSource.countClusters(trustArg());
        clusters = await dataSource.listClusters({
          trust: trustArg(),
          limit: refreshLimit(PAGE_SIZE, clusters.length),
          offset: 0,
        });
        selectedCluster =
          clusters.find((c) => c.clusterId === target.clusterId) ?? null;
        selectedOverlaps = selectedCluster
          ? (
              await Promise.all(
                clusterOverlapGroupIds(selectedCluster).map((g) =>
                  dataSource.partialOverlaps(g),
                ),
              )
            ).flat()
          : [];
      } catch (err) {
        errorStore.reportError(err);
        return {
          ok: true,
          removedFileIds: result.removedFileIds,
          reclaimedBytes: result.reclaimedBytes,
          detail: result.detail.trim() || "삭제되었습니다.",
          rejectCode: null,
        };
      }
    }

    return {
      ok: result.ok,
      removedFileIds: result.removedFileIds,
      reclaimedBytes: result.reclaimedBytes,
      detail: result.detail.trim() || "삭제할 항목이 없습니다.",
      rejectCode: result.rejectCode,
    };
  }

  // "전체 선택" applies to the clusters currently loaded into the list (this
  // page is infinite-scroll paginated via loadMore), not every cluster
  // matching the filter — scrolling in more afterward requires toggling
  // again, same as most incrementally-loaded lists' select-all.
  const allLoadedSelected = $derived(
    clusters.length > 0 &&
      clusters.every((c) => bulkSelectedIds.has(c.clusterId)),
  );

  function toggleSelectAll(): void {
    bulkSelectedIds = allLoadedSelected
      ? new Set()
      : new Set(clusters.map((c) => c.clusterId));
  }

  function toggleBulkSelect(cluster: ContentCluster): void {
    const next = new Set(bulkSelectedIds);
    if (next.has(cluster.clusterId)) {
      next.delete(cluster.clusterId);
    } else {
      next.add(cluster.clusterId);
    }
    bulkSelectedIds = next;
  }

  function clearBulkSelection(): void {
    bulkSelectedIds = new Set();
  }

  function openBulkDialog(mode: DeleteMode): void {
    const targets = clusters.filter((c) => bulkSelectedIds.has(c.clusterId));
    if (targets.length === 0) return;
    bulkMode = mode;
    bulkPlan = planBulkDeletion(targets, mode);
    bulkDialogOpen = true;
  }

  async function confirmBulkDelete(): Promise<void> {
    const plan = bulkPlan;
    if (!plan || plan.perCluster.length === 0) {
      bulkDialogOpen = false;
      return;
    }

    bulkBusy = true;
    let totalBatches = 0;
    let totalRemoved = 0;
    let totalReclaimed = 0;
    let anyFailed = false;

    // 그룹별 삭제를 완전 순차로 돌리면 그룹 수만큼 IPC 왕복이 쌓이고, 무제한
    // Promise.all은 SQLite 쓰기 충돌·휴지통 API 부하를 키운다 — 동시성 2로 제한.
    const BULK_DELETE_CONCURRENCY = 2;
    const queue = [...plan.perCluster];
    async function worker(): Promise<void> {
      let next = queue.shift();
      while (next !== undefined) {
        const { cluster, selected } = next;
        try {
          const result = await deleteClusterFiles(cluster, [...selected], bulkMode);
          totalBatches += result.batches;
          totalRemoved += result.removedFileIds.length;
          totalReclaimed += result.reclaimedBytes;
          if (!result.ok) anyFailed = true;
        } catch (err) {
          errorStore.reportError(err);
          anyFailed = true;
        }
        next = queue.shift();
      }
    }
    await Promise.all(
      Array.from({ length: Math.min(BULK_DELETE_CONCURRENCY, queue.length) }, worker),
    );

    bulkBusy = false;
    bulkDialogOpen = false;
    bulkSelectedIds = new Set();

    if (totalBatches > 0) {
      undoBatchCount = totalBatches;
      const skippedNote =
        plan.skippedClusterIds.length > 0
          ? ` (${plan.skippedClusterIds.length}개 그룹은 건너뜀)`
          : "";
      undoBannerDetail =
        `${plan.perCluster.length}개 그룹에서 ${totalRemoved}개 파일을 정리했습니다 ` +
        `(회수 ${formatBytes(totalReclaimed)})${skippedNote}.`;
      undoBannerOk = !anyFailed;
      undoBannerVisible = true;
    }

    try {
      reclaimable = await dataSource.clusterReclaimableBytes();
      total = await dataSource.countClusters(trustArg());
      clusters = await dataSource.listClusters({
        trust: trustArg(),
        limit: refreshLimit(PAGE_SIZE, clusters.length),
        offset: 0,
      });
      if (
        selectedCluster &&
        !clusters.some((c) => c.clusterId === selectedCluster?.clusterId)
      ) {
        selectedCluster = null;
        selectedOverlaps = [];
      }
    } catch (err) {
      errorStore.reportError(err);
    }
  }

  async function undoDelete(): Promise<void> {
    undoInProgress = true;
    let anyOk = false;
    let lastOutcome: UndoOutcome | null = null;
    for (let i = 0; i < undoBatchCount; i += 1) {
      const outcome = await dataSource.undoLastDelete();
      lastOutcome = outcome;
      if (outcome.ok) {
        anyOk = true;
      } else {
        break; 
      }
    }
    undoInProgress = false;
    if (anyOk) {
      undoBatchCount = 0;
      await reload();
      undoBannerOk = true;
      undoBannerDetail = lastOutcome?.detail ?? "복원되었습니다.";
    } else {
      undoBannerOk = false;
      undoBannerDetail = lastOutcome?.detail ?? "되돌릴 삭제 내역이 없습니다.";
    }
  }

  onMount(() => {
    void reload();
    void (async () => {
      await pollProgress();
      scheduleNextPoll();
    })();
    void (async () => {
      try {
        const settings = await loadSettings();
        scanFolderCount = settings.scanFolders.length;
        indexingEnabled = settings.indexingEnabled;
      } catch {
      }
    })();
    stopFrameSampling = sampleFrames(frameTimer);
    return () => {
      if (pollTimer) clearTimeout(pollTimer);
      if (stopFrameSampling) stopFrameSampling();
    };
  });
</script>

<div class="page">
  <ProgressPanel
    snapshot={progressSnapshot}
    {throughput}
    etaSeconds={eta}
    speedBytesPerSec={speedBytesPerSec}
    speedKnown={speedKnown}
    {stalled}
    reclaimableBytes={reclaimable}
    collapsed={progressCollapsed}
    ontoggle={() => (progressCollapsed = !progressCollapsed)}
    watchedFolderCount={scanFolderCount}
    lastActivityLabel={lastActivityLabel}
    {indexingEnabled}
    onindexingtoggle={() => void toggleIndexing()}
    paused={!indexingEnabled && progressSnapshot !== null && isScanning(progressSnapshot)}
  />

  <div
    class="review"
    class:review--split={selectedCluster}
    class:review--collapsed={failuresOpen && failedCount > 0}
    bind:this={reviewEl}
    style:--list-px={selectedCluster && listWidth !== null
      ? `${listWidth}px`
      : null}
  >
  <section class="review__list">
    <header class="review__header">
      <div>
        <p class="eyebrow">중복 리뷰</p>
        <h1 class="review__title">중복 영상 정리</h1>
      </div>
      <button
        class="btn btn--outline"
        type="button"
        disabled={loading}
        aria-label="화면 초기화"
        onclick={() => void resetUi()}
      >화면 초기화</button>
    </header>

    {#if undoBannerVisible}
      <div class="undo-banner" class:undo-banner--error={!undoBannerOk}>
        <span class="undo-banner__detail">{undoBannerDetail}</span>
        {#if undoBannerOk && undoBatchCount > 0}
          <button
            class="btn btn--ghost undo-banner__action"
            type="button"
            disabled={undoInProgress}
            onclick={() => undoDelete()}
          >실행 취소</button>
        {/if}
        <button
          class="btn btn--ghost undo-banner__close"
          type="button"
          aria-label="닫기"
          onclick={() => (undoBannerVisible = false)}
        >✕</button>
      </div>
    {/if}

    <nav class="review__tabs">
      {#each tabs as tab (tab.id)}
        <button
          class="tab"
          class:tab--active={filter === tab.id}
          type="button"
          title={tab.label}
          aria-label={tab.label}
          onclick={() => setFilter(tab.id)}
        >
          <Icon name={tab.icon} class="tab__icon" />
          <span class="tab__label">
            <span class="tab__label-full">{tab.label}</span>
            <span class="tab__label-short">{tab.shortLabel ?? tab.label}</span>
          </span>
        </button>
      {/each}
      {#if clusters.length > 0}
        <label class="review__select-all">
          <input
            type="checkbox"
            data-testid="select-all-checkbox"
            checked={allLoadedSelected}
            onchange={toggleSelectAll}
          />
          전체 선택
        </label>
      {/if}
      <span class="review__total">{total}개 그룹</span>
    </nav>

    {#if bulkSelectedIds.size > 0}
      <div class="bulk-bar" data-testid="bulk-action-bar">
        <span class="bulk-bar__count">{bulkSelectedIds.size}개 그룹 선택됨</span>
        <button
          class="btn btn--ghost bulk-bar__clear"
          type="button"
          onclick={clearBulkSelection}
        >선택 해제</button>
        <button
          class="btn btn--outline bulk-bar__permanent"
          type="button"
          onclick={() => openBulkDialog("permanent")}
        >영구 삭제</button>
        <button
          class="btn btn--primary bulk-bar__trash"
          type="button"
          data-testid="bulk-delete-trash"
          onclick={() => openBulkDialog("trash")}
        >선택 항목 일괄 삭제</button>
      </div>
    {/if}

    {#if shouldShowIntroOutroBar(hiddenIntroOutroCount)}
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
            인트로/아웃트로 겹침 {hiddenIntroOutroCount}건 숨김
          </span>
        {/if}
      </div>
    {/if}

    <div class="review__scroll">
      {#if loading && total === 0}
        <div class="review__loading" data-testid="cluster-list-loading">
          <span class="review__loading-dot"></span>중복 영상 불러오는 중…
        </div>
      {:else if total === 0 && !loading}
        <div class="empty">
          <p>표시할 중복 영상이 없습니다.</p>
          <p class="empty__hint">탐색할 폴더를 추가하면 인덱싱이 시작됩니다.</p>
          <div class="empty__actions">
            <button
              class="btn btn--primary"
              type="button"
              data-testid="empty-add-folder"
              disabled={addingFolder}
              onclick={() => addScanFolder()}
            >
              {addingFolder ? "폴더 추가 중…" : "폴더 추가"}
            </button>
            <a class="btn btn--outline" href="/options">탐색 폴더 설정</a>
          </div>
          <p class="empty__hint" data-testid="empty-diag-hint">
            예상과 다르게 비어 있거나 문제가 있나요?
            <a href="/options">설정 → 진단 / 로그</a>에서 진단 로그를 내보내
            개발자에게 전달할 수 있어요.
          </p>
          {#if addFolderError}
            <p class="empty__error" data-testid="empty-add-error">{addFolderError}</p>
          {/if}
        </div>
      {:else}
        <VirtualList
          items={visibleClusters}
          rowHeight={ROW_HEIGHT}
          key={(c) => c.clusterId}
          onreachend={loadMore}
          onscroll={onListScroll}
        >
          {#snippet row(cluster: ContentCluster)}
            <div class="review__rowpad">
              <ClusterCard
                {cluster}
                selected={selectedCluster?.clusterId === cluster.clusterId}
                checked={bulkSelectedIds.has(cluster.clusterId)}
                onselect={(c) => selectCluster(c)}
                onchecktoggle={(c) => toggleBulkSelect(c)}
                fetchThumbnail={loadThumbnail}
              />
            </div>
          {/snippet}
        </VirtualList>
      {/if}
    </div>
  </section>

  {#if selectedCluster}
    <div
      class="splitter"
      role="separator"
      aria-orientation="vertical"
      aria-label="목록과 상세 패널 크기 조절"
      title="드래그하여 크기 조절"
      onpointerdown={startResize}
    ></div>
    <section class="review__detail">
      <CompareView
        group={clusterAsGroup(selectedCluster)}
        cluster={selectedCluster}
        overlaps={selectedOverlaps}
        onclose={() => {
          selectedCluster = null;
          selectedOverlaps = [];
        }}
        onexecute={execute}
        fetchThumbnail={loadThumbnail}
      />
    </section>
  {/if}
  </div>

  <FailureAccordion
    tasks={failedTasks}
    {failedCount}
    open={failuresOpen}
    ontoggle={() => (failuresOpen = !failuresOpen)}
  />

  <BulkDeleteDialog
    bind:open={bulkDialogOpen}
    plan={bulkPlan ?? EMPTY_BULK_PLAN}
    mode={bulkMode}
    busy={bulkBusy}
    onconfirm={() => void confirmBulkDelete()}
  />
</div>

<style>
  .page {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }
  .review {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: 1fr;
    




    grid-template-rows: minmax(0, 1fr);
  }
  

  .review--collapsed {
    display: none;
  }
  .review--split {
    


    grid-template-columns: var(--list-px, minmax(300px, 1fr)) 6px minmax(390px, 1fr);
  }
  .splitter {
    cursor: col-resize;
    background: var(--color-hairline);
    transition: background 120ms ease;
    touch-action: none;
  }
  .splitter:hover {
    background: var(--color-primary);
  }
  .review__list {
    display: flex;
    flex-direction: column;
    min-width: 0;
    height: 100%;
    

    container-type: inline-size;
  }
  
  :global(.tab__icon) {
    display: none;
    font-size: 1.05rem;
  }
  .tab__label-short {
    display: none;
  }
  @container (max-width: 567px) {
    .tab__label-full {
      display: none;
    }
    .tab__label-short {
      display: inline;
    }
  }
  @container (max-width: 470px) {
    .tab__label {
      display: none;
    }
    :global(.tab__icon) {
      display: inline-flex;
    }
    .review__total {
      display: none;
    }
  }
  .review__header {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    padding: var(--space-sm) var(--space-md) var(--space-xs);
  }
  .review__title {
    font-size: var(--type-display-lg-size);
    letter-spacing: var(--type-display-lg-tracking);
  }
  .review__tabs {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-xxs);
    padding: 0 var(--space-md) var(--space-xs);
    border-bottom: 1px solid var(--color-hairline);
  }
  .tab {
    padding: var(--space-xxs) var(--space-xs);
    background: transparent;
    border: none;
    border-bottom: 2px solid transparent;
    color: var(--color-body);
    font-family: var(--font-sans);
    font-size: var(--type-title-sm-size);
    

    white-space: nowrap;
    cursor: pointer;
  }
  .tab--active {
    color: var(--color-ink);
    border-bottom-color: var(--color-primary);
  }
  .review__total {
    margin-left: auto;
    color: var(--color-muted);
    font-size: var(--type-body-sm-size);
    white-space: nowrap;
  }
  .review__select-all {
    display: flex;
    align-items: center;
    gap: var(--space-xxxs);
    margin-left: var(--space-sm);
    color: var(--color-body);
    font-size: var(--type-body-sm-size);
    white-space: nowrap;
    cursor: pointer;
  }

  .bulk-bar {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-xs);
    padding: var(--space-xxs) var(--space-md);
    background: var(--color-surface-raised, var(--color-surface));
    border-bottom: 1px solid var(--color-hairline);
    font-size: var(--type-body-sm-size);
    color: var(--color-ink);
  }
  .bulk-bar__count {
    font-weight: var(--font-weight-strong);
  }
  .bulk-bar__clear {
    margin-right: auto;
  }
  .intro-outro-bar {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-sm);
    padding: var(--space-xxs) var(--space-md);
    border-bottom: 1px solid var(--color-hairline);
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
  .review__scroll {
    flex: 1;
    min-height: 0;
    padding: var(--space-xs) var(--space-md);
  }
  .review__rowpad {
    height: 100%;
    padding-bottom: var(--space-xxs);
  }
  .review__detail {
    height: 100%;
    min-height: 0;
    min-width: 0;
  }
  


  .review__loading {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: var(--space-xs);
    height: 100%;
    color: var(--color-muted);
    text-align: center;
  }
  .review__loading-dot {
    width: 8px;
    height: 8px;
    border-radius: var(--radius-full);
    background: var(--color-muted);
    animation: review-loading-pulse 1.4s ease-in-out infinite;
  }
  @keyframes review-loading-pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.3;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .review__loading-dot {
      animation: none;
    }
  }
  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-xs);
    height: 100%;
    color: var(--color-body);
    text-align: center;
  }
  .empty__hint {
    color: var(--color-muted);
    font-size: var(--type-body-sm-size);
  }
  .empty__actions {
    display: flex;
    gap: var(--space-xs);
    margin-top: var(--space-xs);
  }
  .empty__error {
    color: var(--color-semantic-warning, var(--color-muted));
    font-size: var(--type-body-sm-size);
  }
  .undo-banner {
    display: flex;
    align-items: center;
    gap: var(--space-xs);
    padding: var(--space-xxs) var(--space-md);
    background: var(--color-surface-raised, var(--color-surface));
    border-bottom: 1px solid var(--color-hairline);
    font-size: var(--type-body-sm-size);
    color: var(--color-ink);
  }
  .undo-banner--error {
    color: var(--color-danger, #c81d25);
  }
  .undo-banner__detail {
    flex: 1;
    min-width: 0;
  }
  .undo-banner__action,
  .undo-banner__close {
    flex-shrink: 0;
  }
</style>
