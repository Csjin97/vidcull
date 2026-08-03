<script lang="ts">
  import { lockStore } from "$lib/stores/lockStore.svelte";

  let pw = $state("");
  let errorMsg = $state("");
  let shaking = $state(false);
  let inputEl: HTMLInputElement | null = $state(null);

  $effect(() => {
    if (lockStore.isLocked && inputEl) {
      requestAnimationFrame(() => inputEl?.focus());
    }
  });

  $effect(() => {
    if (pw.length > 0) errorMsg = "";
  });

  async function submit(): Promise<void> {
    if (!pw) return;
    const ok = await lockStore.verify(pw);
    pw = "";
    if (ok) {
      errorMsg = "";
      lockStore.unlock();
    } else {
      errorMsg = "비밀번호가 틀렸습니다";
      shaking = true;
      setTimeout(() => {
        shaking = false;
      }, 450);
    }
  }

  function onKeydown(e: KeyboardEvent): void {
    if (e.key === "Enter") void submit();
    if (e.key === "Tab") {
      e.preventDefault(); 
    }
  }
</script>

{#if lockStore.isLocked}
  <div
    class="overlay"
    role="dialog"
    aria-modal="true"
    aria-labelledby="lock-title"
    tabindex="-1"
    onkeydown={onKeydown}
  >
    <div class="panel" class:panel--shake={shaking}>
      
      <svg
        class="lock-icon"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="2"
        stroke-linecap="round"
        stroke-linejoin="round"
        aria-hidden="true"
      >
        <rect x="3" y="11" width="18" height="11" rx="2" ry="2"></rect>
        <path d="M7 11V7a5 5 0 0 1 10 0v4"></path>
      </svg>

      <h2 id="lock-title" class="panel__title">vidcull</h2>
      <p class="panel__subtitle">화면이 잠겨 있습니다</p>

      {#if errorMsg}
        <p class="panel__error" role="alert">{errorMsg}</p>
      {/if}

      <input
        bind:this={inputEl}
        type="password"
        class="input"
        placeholder="비밀번호"
        bind:value={pw}
        autocomplete="current-password"
        aria-label="잠금 해제 비밀번호"
        onkeydown={(e) => e.key === "Enter" && void submit()}
      />

      <button class="btn btn--primary unlock-btn" type="button" onclick={() => void submit()}>
        잠금 해제
      </button>

      <p class="panel__disclaimer">
        주변 시선을 가리는 화면 잠금입니다. 디스크 데이터는 암호화되지 않습니다.
      </p>
    </div>
  </div>
{/if}

<style>
  .overlay {
    position: fixed;
    inset: 0;
    z-index: 999;
    display: flex;
    align-items: center;
    justify-content: center;
    background: rgb(0 0 0 / 0.92);
    backdrop-filter: blur(6px);
  }

  .panel {
    width: 100%;
    max-width: 22rem;
    margin: var(--space-md);
    padding: var(--space-lg) var(--space-md);
    background: var(--color-canvas-elevated);
    border: 1px solid var(--color-hairline);
    border-top: 3px solid var(--color-primary);
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--space-xs);
  }

  @keyframes shake {
    0%,
    100% {
      transform: translateX(0);
    }
    20%,
    60% {
      transform: translateX(-10px);
    }
    40%,
    80% {
      transform: translateX(10px);
    }
  }

  .panel--shake {
    animation: shake 0.45s ease-in-out;
  }

  .lock-icon {
    width: 40px;
    height: 40px;
    color: var(--color-primary);
    margin-bottom: var(--space-xxs);
  }

  .panel__title {
    margin: 0;
    font-size: var(--type-title-md-size);
    font-weight: var(--font-weight-strong);
    color: var(--color-ink);
    letter-spacing: 0.04em;
  }

  .panel__subtitle {
    margin: 0;
    color: var(--color-body);
    font-size: var(--type-body-sm-size);
  }

  .panel__error {
    margin: 0;
    color: var(--color-semantic-warning);
    font-size: var(--type-body-sm-size);
    text-align: center;
  }

  
  .input {
    width: 100%;
    max-width: 18rem;
  }

  .unlock-btn {
    width: 100%;
    max-width: 18rem;
  }

  .panel__disclaimer {
    margin: var(--space-xxs) 0 0;
    color: var(--color-muted);
    font-size: var(--type-caption-size);
    line-height: 1.5;
    text-align: center;
  }
</style>
