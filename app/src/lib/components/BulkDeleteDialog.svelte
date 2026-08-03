<script lang="ts">
  import { Dialog } from "bits-ui";
  import { formatBytes } from "$lib/model/format";
  import type { BulkDeletionPlan, DeleteMode } from "$lib/model/safe-delete";

  let {
    open = $bindable(false),
    plan,
    mode,
    busy = false,
    onconfirm,
  }: {
    open?: boolean;
    plan: BulkDeletionPlan;
    mode: DeleteMode;
    busy?: boolean;
    onconfirm: () => void;
  } = $props();

  let acknowledged = $state(false);

  $effect(() => {
    if (open) acknowledged = false;
  });

  const confirmLabel = $derived(
    mode === "permanent" ? "영구 삭제" : "휴지통으로 이동",
  );
  const canProceed = $derived(plan.perCluster.length > 0);
  const requiresAck = $derived(mode === "permanent" && canProceed);
  const confirmDisabled = $derived(
    busy || !canProceed || (requiresAck && !acknowledged),
  );
</script>

<Dialog.Root bind:open>
  <Dialog.Portal>
    <Dialog.Overlay class="dlg__overlay" />
    <Dialog.Content class="dlg__content">
      <Dialog.Title class="dlg__title">일괄 {confirmLabel} 확인</Dialog.Title>
      <Dialog.Description class="dlg__desc">
        {#if canProceed}
          {plan.perCluster.length}개 그룹에서 {plan.toDelete.length}개 파일을
          삭제하고 {plan.kept.length}개를 보존합니다.
        {:else}
          삭제할 수 있는 그룹이 없습니다.
        {/if}
      </Dialog.Description>

      {#if canProceed}
        <div class="dlg__reclaim">
          회수 용량 {formatBytes(plan.reclaimedBytes)}
        </div>
      {/if}

      {#if plan.skippedClusterIds.length > 0}
        <p class="dlg__issue dlg__issue--warn">
          ⚠ {plan.skippedClusterIds.length}개 그룹은 최적 사본을 자동으로
          판단할 수 없어 건너뜁니다 — 개별적으로 검토해 주세요.
        </p>
      {/if}

      {#if mode === "permanent"}
        <p class="dlg__issue dlg__issue--error">
          영구 삭제는 되돌릴 수 없습니다. 휴지통으로 이동하지 않습니다.
        </p>
      {/if}

      {#if requiresAck}
        <label class="dlg__ack">
          <input type="checkbox" bind:checked={acknowledged} />
          위험을 이해했으며 계속 진행합니다.
        </label>
      {/if}

      <div class="dlg__actions">
        <Dialog.Close class="btn btn--outline">취소</Dialog.Close>
        <button
          class="btn {mode === 'permanent' ? 'btn--danger' : 'btn--primary'}"
          disabled={confirmDisabled}
          onclick={onconfirm}
        >
          {busy ? "처리 중…" : `${confirmLabel} (${plan.perCluster.length}개 그룹)`}
        </button>
      </div>
    </Dialog.Content>
  </Dialog.Portal>
</Dialog.Root>

<style>
  :global(.dlg__overlay) {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.62);
    z-index: 50;
  }
  :global(.dlg__content) {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    width: min(560px, calc(100vw - var(--space-lg)));
    max-height: calc(100vh - var(--space-lg));
    overflow-y: auto;
    padding: var(--space-md);
    background: var(--color-canvas-elevated);
    border: 1px solid var(--color-hairline);
    border-radius: var(--radius-none);
    color: var(--color-ink);
    z-index: 51;
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
  }
  :global(.dlg__title) {
    margin: 0;
    font-size: var(--type-display-md-size);
    font-weight: var(--font-weight-display);
  }
  :global(.dlg__desc) {
    margin: 0;
    color: var(--color-body);
    font-size: var(--type-body-sm-size);
  }
  :global(.dlg__reclaim) {
    color: var(--color-ink);
    font-weight: var(--font-weight-strong);
  }
  :global(.dlg__issue) {
    margin: 0;
    font-size: var(--type-body-sm-size);
  }
  :global(.dlg__issue--error) {
    color: var(--color-semantic-warning);
    font-weight: var(--font-weight-strong);
  }
  :global(.dlg__issue--warn) {
    color: var(--color-accent-yellow);
  }
  :global(.dlg__ack) {
    display: flex;
    align-items: center;
    gap: var(--space-xxs);
    font-size: var(--type-body-sm-size);
    color: var(--color-ink);
  }
  :global(.dlg__actions) {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-xs);
    margin-top: var(--space-xxs);
  }
</style>
