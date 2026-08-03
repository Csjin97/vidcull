<script module lang="ts">
  // Shared across every Thumbnail instance. VirtualList unmounts a row's
  // component each time it scrolls out of the overscan window and remounts
  // it on scroll-back, so without this every remount re-issues the same IPC
  // thumbnail fetch. Only successful (non-null) results stay cached — a
  // still-generating or failed fetch is evicted so the next remount retries
  // naturally instead of permanently freezing on "no thumbnail".
  const cache = new Map<number, Promise<string | null>>();

  export function clearThumbnailCache(): void {
    cache.clear();
  }
</script>

<script lang="ts">
  let {
    src,
    alt,
    fileId = null,
    fetchThumbnail,
  }: {
    src: string | null;
    alt: string;
    fileId?: number | null;
    fetchThumbnail?: (fileId: number) => Promise<string | null>;
  } = $props();

  let lazyUrl = $state<string | null>(null);
  let pending = $state(false);
  let requestedFor: number | null = null;

  $effect(() => {
    if (src !== null || fileId === null || !fetchThumbnail) return;
    if (requestedFor === fileId) return;
    requestedFor = fileId;
    lazyUrl = null;
    pending = true;
    const id = fileId;
    let cancelled = false;

    let cached = cache.get(id);
    if (!cached) {
      cached = fetchThumbnail(id).then((url) => {
        if (url === null) cache.delete(id);
        return url;
      });
      cached.catch(() => cache.delete(id));
      cache.set(id, cached);
    }

    cached
      .then((url) => {
        if (!cancelled && requestedFor === id) {
          lazyUrl = url;
          pending = false;
        }
      })
      .catch(() => {
        if (!cancelled && requestedFor === id) {
          pending = false;
        }
      });
    return () => {
      cancelled = true;
    };
  });

  const display = $derived(src ?? lazyUrl);
</script>

<div class="thumb">
  {#if display}
    <img class="thumb__img" src={display} {alt} loading="lazy" decoding="async" />
  {:else if pending}
    <div class="thumb__placeholder thumb__placeholder--pending" role="img" aria-label={alt}>
      <span class="thumb__pending-dot"></span>준비 중
    </div>
  {:else}
    <div class="thumb__placeholder" role="img" aria-label={alt}>▶</div>
  {/if}
</div>

<style>
  .thumb {
    position: relative;
    aspect-ratio: 16 / 9;
    background: var(--color-canvas-elevated);
    overflow: hidden;
    border-radius: var(--radius-none);
  }
  .thumb__img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
  }
  .thumb__placeholder {
    width: 100%;
    height: 100%;
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--color-muted);
    font-size: var(--type-display-md-size);
  }
  

  .thumb__placeholder--pending {
    flex-direction: column;
    gap: var(--space-xxs);
    background: var(--color-canvas-elevated);
    color: var(--color-muted);
    font-size: var(--type-caption-size);
  }
  .thumb__pending-dot {
    width: 8px;
    height: 8px;
    border-radius: var(--radius-full);
    background: var(--color-muted);
    animation: thumb-pulse 1.4s ease-in-out infinite;
  }
  @keyframes thumb-pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.3;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .thumb__pending-dot {
      animation: none;
    }
  }
</style>
