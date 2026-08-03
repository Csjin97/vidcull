
<script lang="ts">
  import { errorStore } from "$lib/stores/errorStore.svelte";
  import ThrowingChild from "./throwing-child.fixture.svelte";

  let { shouldThrow = false }: { shouldThrow?: boolean } = $props();
</script>

<svelte:boundary onerror={(error) => errorStore.reportError(error)}>
  {#if shouldThrow}
    <ThrowingChild />
  {:else}
    <div data-testid="boundary-content">정상 콘텐츠</div>
  {/if}
  {#snippet failed(_error, reset)}
    <div data-testid="boundary-fallback" role="alert">
      <p>이 화면을 표시하는 중 오류가 발생했습니다.</p>
      <button type="button" onclick={reset}>다시 시도</button>
    </div>
  {/snippet}
</svelte:boundary>
