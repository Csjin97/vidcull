<script lang="ts" generics="T">
  import type { Snippet } from "svelte";
  import { computeWindow, isNearEnd } from "$lib/virtual/window";

  let {
    items,
    rowHeight,
    overscan = 4,
    onreachend,
    onscroll,
    row,
    key,
  }: {
    items: T[];
    rowHeight: number;
    overscan?: number;
    onreachend?: () => void;

    onscroll?: (scrollTop: number) => void;
    row: Snippet<[T, number]>;
    // Identity key per item, e.g. `(c) => c.clusterId`. Without one, rows key
    // by absolute position — fine for a static list, but if `items` is ever
    // reordered/spliced (a reload, a delete) Svelte's keyed reconciliation
    // will reuse a row's DOM/component for whatever item now sits at that
    // position instead of recognizing it as a different item.
    key?: (item: T, index: number) => string | number;
  } = $props();

  let viewport = $state<HTMLDivElement | null>(null);
  let scrollTop = $state(0);
  let viewportHeight = $state(0);

  const win = $derived(
    computeWindow({
      scrollTop,
      viewportHeight,
      rowHeight,
      count: items.length,
      overscan,
    }),
  );

  function handleScroll(): void {
    if (!viewport) return;
    scrollTop = viewport.scrollTop;
    onscroll?.(scrollTop);
    if (
      onreachend &&
      isNearEnd({ scrollTop, viewportHeight, rowHeight, count: items.length })
    ) {
      onreachend();
    }
  }

  $effect(() => {
    if (!viewport) return;
    viewportHeight = viewport.clientHeight;
    const ro = new ResizeObserver(() => {
      if (viewport) viewportHeight = viewport.clientHeight;
    });
    ro.observe(viewport);
    return () => ro.disconnect();
  });
</script>

<div class="vlist" bind:this={viewport} onscroll={handleScroll}>
  <div class="vlist__sizer" style="height: {win.totalHeight}px">
    <div class="vlist__window" style="transform: translateY({win.offsetY}px)">
      {#each items.slice(win.startIndex, win.endIndex) as item, i (key ? key(item, win.startIndex + i) : win.startIndex + i)}
        <div class="vlist__row" style="height: {rowHeight}px">
          {@render row(item, win.startIndex + i)}
        </div>
      {/each}
    </div>
  </div>
</div>

<style>
  .vlist {
    height: 100%;
    overflow-y: auto;
    overflow-x: hidden;
  }
  .vlist__sizer {
    position: relative;
    width: 100%;
  }
  .vlist__window {
    position: absolute;
    top: 0;
    left: 0;
    width: 100%;
  }
  .vlist__row {
    width: 100%;
  }
</style>
