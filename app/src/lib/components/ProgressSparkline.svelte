<script lang="ts">
  import { sparklinePath } from "$lib/model/progress";

  let {
    values,
    width = 240,
    height = 48,
    label = "처리량 추이",
  }: {
    values: number[];
    width?: number;
    height?: number;
    label?: string;
  } = $props();

  const path = $derived(sparklinePath(values, width, height));
  const areaPath = $derived(
    path === ""
      ? ""
      : `${path} L ${width},${height} L 0,${height} Z`,
  );
</script>

<svg
  class="spark"
  viewBox="0 0 {width} {height}"
  preserveAspectRatio="none"
  role="img"
  aria-label={label}
  data-testid="sparkline"
>
  {#if path}
    <path class="spark__area" d={areaPath} />
    <path class="spark__line" d={path} />
  {:else}
    <line
      class="spark__empty"
      x1="0"
      y1={height / 2}
      x2={width}
      y2={height / 2}
    />
  {/if}
</svg>

<style>
  .spark {
    display: block;
    width: 100%;
    height: 100%;
  }
  .spark__line {
    fill: none;
    stroke: var(--color-primary);
    stroke-width: 1.5;
    vector-effect: non-scaling-stroke;
    stroke-linejoin: round;
    stroke-linecap: round;
  }
  .spark__area {
    fill: var(--color-primary);
    opacity: 0.12;
  }
  .spark__empty {
    stroke: var(--color-hairline);
    stroke-width: 1;
    stroke-dasharray: 3 3;
    vector-effect: non-scaling-stroke;
  }
</style>
