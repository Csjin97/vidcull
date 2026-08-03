<script lang="ts">
  import { EXPECTED_PROTOCOL_VERSION, type PingResult } from "../../daemon";

  let { ping }: { ping: PingResult } = $props();

  const mismatch = $derived(ping.ok && !ping.compatible ? ping : null);
</script>

{#if mismatch}
  <div
    class="gate"
    role="alertdialog"
    aria-modal="true"
    aria-labelledby="protocol-gate-title"
    aria-describedby="protocol-gate-body"
  >
    <div class="gate__panel">
      <h2 id="protocol-gate-title" class="gate__title">프로토콜 버전 불일치</h2>
      <p id="protocol-gate-body" class="gate__body">
        데몬은 프로토콜 <strong>v{mismatch.protocolVersion}</strong>, 앱은
        <strong>v{EXPECTED_PROTOCOL_VERSION}</strong>을 사용하고 있습니다. 서로
        다른 버전끼리는 데이터가 잘못 해석될 수 있어 모든 요청을 차단했습니다.
      </p>
      <p class="gate__hint">
        앱과 데몬을 같은 버전으로 업데이트한 뒤 데몬을 재시작하면 자동으로
        다시 연결됩니다.
      </p>
    </div>
  </div>
{/if}

<style>
  .gate {
    position: fixed;
    inset: 0;
    z-index: 100;
    display: flex;
    align-items: center;
    justify-content: center;
    background: rgb(0 0 0 / 0.55);
    backdrop-filter: blur(2px);
  }
  .gate__panel {
    max-width: 28rem;
    margin: var(--space-md);
    padding: var(--space-md) var(--space-lg);
    background: var(--color-canvas-elevated);
    border: 1px solid var(--color-hairline);
    border-left: 3px solid var(--color-semantic-warning);
    border-radius: var(--radius-md, 8px);
    box-shadow: 0 8px 32px rgb(0 0 0 / 0.35);
  }
  .gate__title {
    margin: 0 0 var(--space-xs);
    font-size: var(--type-title-md-size);
    font-weight: var(--font-weight-strong);
    color: var(--color-ink);
  }
  .gate__body {
    margin: 0 0 var(--space-xs);
    color: var(--color-body);
    font-size: var(--type-body-md-size);
    line-height: 1.5;
  }
  .gate__hint {
    margin: 0;
    color: var(--color-muted);
    font-size: var(--type-caption-size);
    line-height: 1.5;
  }
</style>
