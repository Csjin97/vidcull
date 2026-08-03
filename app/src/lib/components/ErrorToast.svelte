<script lang="ts">
  import { errorStore } from "../stores/errorStore.svelte";

  let copiedId = $state<number | null>(null);

  function copyToClipboard(id: number, message: string): void {
    navigator.clipboard.writeText(message).then(() => {
      copiedId = id;
      setTimeout(() => {
        if (copiedId === id) copiedId = null;
      }, 2000);
    }).catch((err: unknown) => {
      console.error("클립보드 복사 실패:", err);
    });
  }
</script>

{#if errorStore.errors.length > 0}
  <div class="toast-stack" role="alert" aria-live="assertive" aria-label="오류 알림">
    {#each errorStore.errors as entry (entry.id)}
      <div
        class="toast"
        class:toast--error={entry.severity === "error"}
        class:toast--warning={entry.severity === "warning"}
        class:toast--info={entry.severity === "info"}
        data-testid="error-toast"
      >
        <span class="toast__msg">{entry.message}</span>
        {#if entry.count > 1}
          <span class="toast__count" aria-label="{entry.count}회 반복">×{entry.count}</span>
        {/if}
        <div class="toast__actions">
          <button
            class="toast__btn"
            class:toast__btn--copied={copiedId === entry.id}
            type="button"
            aria-label="오류 메시지 클립보드에 복사"
            onclick={() => copyToClipboard(entry.id, entry.message)}
          >
            {copiedId === entry.id ? "복사 완료" : "로그 복사"}
          </button>
          <button
            class="toast__btn toast__btn--dismiss"
            type="button"
            aria-label="오류 알림 닫기"
            onclick={() => errorStore.dismiss(entry.id)}
          >
            ✕
          </button>
        </div>
      </div>
    {/each}
  </div>
{/if}

<style>
  .toast-stack {
    position: fixed;
    bottom: var(--space-md);
    right: var(--space-md);
    z-index: 50; 
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
    max-width: 28rem;
    
    pointer-events: none;
  }
  .toast {
    display: flex;
    align-items: flex-start;
    gap: var(--space-xs);
    padding: var(--space-xs) var(--space-sm);
    background: var(--color-canvas-elevated);
    border: 1px solid var(--color-hairline);
    border-left: 3px solid var(--color-muted);
    border-radius: var(--radius-sm, 6px);
    box-shadow: 0 4px 16px rgb(0 0 0 / 0.25);
    pointer-events: auto;
  }
  .toast--error {
    border-left-color: var(--color-danger, #c81d25);
  }
  .toast--warning {
    border-left-color: var(--color-semantic-warning);
  }
  .toast--info {
    border-left-color: var(--color-semantic-info);
  }
  .toast__msg {
    flex: 1;
    color: var(--color-ink);
    font-family: var(--font-sans);
    font-size: var(--type-body-sm-size);
    line-height: 1.5;
    word-break: break-word;
  }
  .toast__count {
    color: var(--color-muted);
    font-size: var(--type-caption-size);
    white-space: nowrap;
    align-self: center;
    flex-shrink: 0;
  }
  .toast__actions {
    display: flex;
    gap: var(--space-xxs);
    align-items: flex-start;
    flex-shrink: 0;
  }
  .toast__btn {
    background: transparent;
    border: 1px solid var(--color-hairline);
    color: var(--color-muted);
    padding: 2px 8px;
    border-radius: 2px;
    font-size: 11px;
    font-family: var(--font-sans);
    cursor: pointer;
    transition: all 120ms ease;
    white-space: nowrap;
  }
  .toast__btn:hover {
    color: var(--color-ink);
    border-color: var(--color-ink);
  }
  .toast__btn--copied {
    background: #2a9d8f;
    color: white;
    border-color: transparent;
  }
  .toast__btn--dismiss {
    padding: 2px 6px;
  }
</style>
