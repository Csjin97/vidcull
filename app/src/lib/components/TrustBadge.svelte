<script lang="ts">
  import { trustClass, trustLabel, trustShortLabel } from "$lib/model/format";
  import type { TrustLevel } from "$lib/model/types";

  let {
    trust,
    collapseAt,
  }: { trust: TrustLevel; collapseAt?: 470 | 428 } = $props();

  const full = $derived(trustLabel(trust));
  const short = $derived(trustShortLabel(trust));
  const collapsible = $derived(collapseAt !== undefined && short !== full);
</script>

{#if collapsible}
  <span
    class="badge badge--{trustClass(trust)}"
    class:badge--at470={collapseAt === 470}
    class:badge--at428={collapseAt === 428}
  >
    <span class="badge__full">{full}</span>
    <span class="badge__short">{short}</span>
  </span>
{:else}
  <span class="badge badge--{trustClass(trust)}">{full}</span>
{/if}

<style>
  .badge__short {
    display: none;
  }
  @container (max-width: 470px) {
    .badge--at470 .badge__full {
      display: none;
    }
    .badge--at470 .badge__short {
      display: inline;
    }
  }
  @container (max-width: 428px) {
    .badge--at428 .badge__full {
      display: none;
    }
    .badge--at428 .badge__short {
      display: inline;
    }
  }
</style>
