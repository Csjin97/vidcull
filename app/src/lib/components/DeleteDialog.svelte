<script lang="ts">
  import { Dialog } from "bits-ui";
  import { fileName, formatBytes } from "$lib/model/format";
  import type { DeleteMode, DeletionAssessment } from "$lib/model/safe-delete";

  let {
    open = $bindable(false),
    assessment,
    mode,
    busy = false,
    onconfirm,
  }: {
    open?: boolean;
    assessment: DeletionAssessment;
    mode: DeleteMode;
    busy?: boolean;
    onconfirm: () => void;
  } = $props();

  let acknowledged = $state(false);

  $effect(() => {
    if (open) acknowledged = false;
  });

  const errors = $derived(assessment.issues.filter((i) => i.level === "error"));
  const warnings = $derived(
    assessment.issues.filter((i) => i.level === "warning"),
  );
  const confirmLabel = $derived(
    mode === "permanent" ? "영구 삭제" : "휴지통으로 이동",
  );
  const confirmDisabled = $derived(
    busy ||
      !assessment.canProceed ||
      (assessment.requiresExtraConfirm && !acknowledged),
  );
</script>

<Dialog.Root bind:open>
  <Dialog.Portal>
    <Dialog.Overlay class="dlg__overlay" />
    <Dialog.Content class="dlg__content">
      <Dialog.Title class="dlg__title">{confirmLabel} 확인</Dialog.Title>
      <Dialog.Description class="dlg__desc">
        {assessment.plan?.toDelete.length ?? 0}개 파일을 삭제하고
        {assessment.plan?.kept.length ?? 0}개를 보존합니다.
      </Dialog.Description>

      {#if assessment.plan}
        <div class="dlg__reclaim">
          회수 용량 {formatBytes(assessment.plan.reclaimedBytes)}
        </div>
        <ul class="dlg__list">
          {#each assessment.plan.toDelete as f (f.fileId)}
            <li class="dlg__row dlg__row--del">
              <span>삭제 · {fileName(f.path)}</span>
              <span>{formatBytes(f.sizeBytes)}</span>
            </li>
          {/each}
          {#each assessment.plan.kept as f (f.fileId)}
            <li class="dlg__row dlg__row--keep">
              <span>보존 · {fileName(f.path)}</span>
            </li>
          {/each}
        </ul>
      {/if}

      {#each errors as issue (issue.code)}
        <p class="dlg__issue dlg__issue--error">{issue.message}</p>
      {/each}
      {#each warnings as issue (issue.code)}
        <p class="dlg__issue dlg__issue--warn">⚠ {issue.message}</p>
      {/each}

      {#if assessment.requiresExtraConfirm && assessment.canProceed}
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
          {busy ? "처리 중…" : confirmLabel}
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
  :global(.dlg__list) {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-xxxs);
    max-height: 220px;
    overflow-y: auto;
  }
  :global(.dlg__row) {
    display: flex;
    justify-content: space-between;
    gap: var(--space-xs);
    padding: var(--space-xxxs) var(--space-xxs);
    font-size: var(--type-body-sm-size);
    border-left: 2px solid transparent;
  }
  :global(.dlg__row--del) {
    border-left-color: var(--color-semantic-warning);
    color: var(--color-ink);
  }
  :global(.dlg__row--keep) {
    border-left-color: var(--color-semantic-success);
    color: var(--color-body);
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
