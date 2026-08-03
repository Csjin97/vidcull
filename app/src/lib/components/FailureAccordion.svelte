<script lang="ts">
  import type { FailedTask } from "$lib/model/types";

  let {
    tasks,
    failedCount = 0,
    open = false,
    ontoggle,
  }: {
    tasks: FailedTask[];
    failedCount?: number;
    open?: boolean;
    ontoggle?: () => void;
  } = $props();

  const count = $derived(Math.max(failedCount, tasks.length));


  function fileName(path: string): string {
    if (path === "") return "(알 수 없는 경로)";
    const i = path.lastIndexOf("/");
    return i >= 0 ? path.slice(i + 1) : path;
  }

  let expandedTasks = $state(new Set<number>());
  let copiedTaskId = $state<number | null>(null);

  function toggleExpand(taskId: number) {
    if (expandedTasks.has(taskId)) {
      expandedTasks.delete(taskId);
    } else {
      expandedTasks.add(taskId);
    }
    expandedTasks = new Set(expandedTasks);
  }

  function copyToClipboard(taskId: number, text: string) {
    navigator.clipboard.writeText(text).then(() => {
      copiedTaskId = taskId;
      setTimeout(() => {
        if (copiedTaskId === taskId) {
          copiedTaskId = null;
        }
      }, 2000);
    }).catch(err => {
      console.error("Failed to copy:", err);
    });
  }
</script>

{#if count > 0}
  <section class="failures" class:failures--open={open}>
    <button
      class="failures__toggle"
      type="button"
      aria-expanded={open}
      aria-label={open ? "실패 목록 접기" : "실패 목록 펼치기"}
      onclick={() => ontoggle?.()}
    >
      <span class="failures__chevron" aria-hidden="true">{open ? "▾" : "▸"}</span>
      <span class="failures__title">실패 {count}건</span>
      <span class="failures__hint">인덱싱하지 못한 파일</span>
    </button>

    {#if open}
      {#if tasks.length > 0}
        <ul class="failures__list">
          {#each tasks as task (task.taskId)}
            <li class="failures__row">
              <div class="failures__summary">
                <span class="failures__name" title={task.path}>{fileName(task.path)}</span>
                <span class="failures__reason-summary" title={task.reason}>
                  {task.reason.length > 55 ? task.reason.slice(0, 55) + "..." : task.reason}
                </span>
                <span class="failures__attempts">{task.attempts}회 시도</span>
                <button
                  class="failures__action-btn"
                  type="button"
                  onclick={() => toggleExpand(task.taskId)}
                >
                  {expandedTasks.has(task.taskId) ? "닫기" : "상세보기"}
                </button>
              </div>

              {#if expandedTasks.has(task.taskId)}
                <div class="failures__detail">
                  <div class="failures__detail-header">
                    <span class="failures__detail-title">전체 에러 로그</span>
                    <button
                      class="failures__copy-btn"
                      type="button"
                      aria-label={`${fileName(task.path)} 에러 로그 복사`}
                      class:failures__copy-btn--success={copiedTaskId === task.taskId}
                      onclick={() => copyToClipboard(task.taskId, task.reason)}
                    >
                      {copiedTaskId === task.taskId ? "복사 완료!" : "로그 복사"}
                    </button>
                  </div>
                  <pre class="failures__log" title="에러 상세 로그">{task.reason}</pre>
                </div>
              {/if}
            </li>
          {/each}
        </ul>
      {:else}
        <p class="failures__pending">실패 목록을 불러오는 중…</p>
      {/if}
    {/if}
  </section>
{/if}

<style>
  .failures {
    border: 1px solid var(--color-hairline);
    border-radius: var(--radius-sm, 6px);
    

    background: var(--color-canvas-elevated);
    


    position: relative;
    z-index: 1;
  }
  

  .failures--open {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
  .failures__toggle {
    display: flex;
    align-items: baseline;
    gap: var(--space-xs);
    width: 100%;
    padding: var(--space-xs) var(--space-sm);
    background: transparent;
    border: none;
    color: var(--color-ink);
    font-family: var(--font-sans);
    font-size: var(--type-title-sm-size);
    cursor: pointer;
    text-align: left;
  }
  .failures__chevron {
    color: var(--color-danger, #c81d25);
  }
  .failures__title {
    font-weight: 600;
    color: var(--color-danger, #c81d25);
  }
  .failures__hint {
    color: var(--color-muted);
    font-size: var(--type-body-sm-size);
  }
  .failures__list {
    list-style: none;
    margin: 0;
    padding: 0 var(--space-sm) var(--space-xs);
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }
  .failures__row {
    padding: var(--space-xs) 0;
    border-top: 1px solid var(--color-hairline);
    font-size: var(--type-body-sm-size);
  }
  .failures__summary {
    display: grid;
    grid-template-columns: minmax(120px, 1.2fr) minmax(160px, 2fr) auto auto;
    gap: var(--space-xs);
    align-items: baseline;
    width: 100%;
  }
  .failures__name {
    color: var(--color-ink);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .failures__reason-summary {
    color: var(--color-body);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .failures__attempts {
    color: var(--color-muted);
    white-space: nowrap;
  }
  .failures__action-btn {
    background: transparent;
    border: 1px solid var(--color-hairline);
    color: var(--color-muted);
    padding: 2px 8px;
    border-radius: 2px;
    font-size: 11px;
    cursor: pointer;
    transition: all 120ms ease;
    white-space: nowrap;
  }
  .failures__action-btn:hover {
    color: var(--color-ink);
    border-color: var(--color-ink);
  }
  .failures__detail {
    margin-top: var(--space-xs);
    padding: var(--space-xs);
    background: var(--color-canvas-elevated, #242424);
    border-radius: var(--radius-sm, 4px);
    border: 1px solid var(--color-hairline);
    user-select: text;
    -webkit-user-select: text;
  }
  .failures__detail-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: var(--space-xs);
    font-size: var(--type-body-sm-size);
  }
  .failures__detail-title {
    font-weight: 600;
    color: var(--color-muted);
  }
  .failures__copy-btn {
    background: var(--color-canvas, #1a1a1a);
    border: 1px solid var(--color-hairline);
    color: var(--color-ink);
    padding: 2px 8px;
    border-radius: 2px;
    font-size: 11px;
    cursor: pointer;
    transition: all 120ms ease;
  }
  .failures__copy-btn:hover {
    border-color: var(--color-ink);
  }
  .failures__copy-btn--success {
    background: #2a9d8f;
    color: white;
    border-color: transparent;
  }
  .failures__log {
    margin: 0;
    font-family: var(--font-mono, monospace);
    font-size: 12px;
    color: var(--color-ink);
    white-space: pre-wrap;
    word-break: break-all;
    user-select: text;
    -webkit-user-select: text;
  }
  .failures__pending {
    margin: 0;
    padding: var(--space-xxs) var(--space-sm) var(--space-xs);
    color: var(--color-muted);
    font-size: var(--type-body-sm-size);
  }
</style>
