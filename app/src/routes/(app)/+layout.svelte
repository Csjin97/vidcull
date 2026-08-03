<script lang="ts">
  import { onMount } from "svelte";
  import { page } from "$app/state";
  import Icon from "$lib/components/Icon.svelte";
  import ProtocolGate from "$lib/components/ProtocolGate.svelte";
  import ErrorToast from "$lib/components/ErrorToast.svelte";
  import ScreenLock from "$lib/components/ScreenLock.svelte";
  import { pingDaemon } from "$lib/ipc/tauri";
  import { statusClass, statusLabel, type PingResult } from "../../daemon";
  import { errorStore } from "$lib/stores/errorStore.svelte";
  import { lockStore } from "$lib/stores/lockStore.svelte";

  let { children } = $props();

  let ping = $state<PingResult>({ ok: false, error: "연결 확인 전" });

  const PING_INTERVAL_MS = 3000;

  onMount(() => {
    let timer: ReturnType<typeof setInterval> | null = null;
    void pingDaemon().then((r) => (ping = r));
    timer = setInterval(() => {
      void pingDaemon().then((r) => (ping = r));
    }, PING_INTERVAL_MS);
    return () => {
      if (timer) clearInterval(timer);
    };
  });

  const nav = [
    { href: "/", label: "중복 리뷰", icon: "review" },
    { href: "/options", label: "옵션", icon: "options" },
    { href: "/licenses", label: "라이선스", icon: "license" },
  ];

  const NAV_COLLAPSE_KEY = "vidcull.navCollapsed";
  let navCollapsed = $state(false);

  onMount(() => {
    if (typeof localStorage !== "undefined") {
      navCollapsed = localStorage.getItem(NAV_COLLAPSE_KEY) === "1";
    }
  });

  onMount(() => {
    lockStore.loadConfig();
    if (lockStore.enabled) lockStore.lock();
  });

  $effect(() => {
    if (typeof window === "undefined") return;
    if (!lockStore.enabled) return;

    const idleMs = lockStore.idleMinutes * 60 * 1000;
    let idleTimer: ReturnType<typeof setTimeout> | null = null;

    function resetIdle(): void {
      if (idleTimer) clearTimeout(idleTimer);
      if (idleMs > 0) {
        idleTimer = setTimeout(() => {
          lockStore.lock();
        }, idleMs);
      }
    }

    function onBlur(): void {
      if (lockStore.lockOnBlur) lockStore.lock();
    }

    function onVisibilityChange(): void {
      if (document.visibilityState === "hidden" && lockStore.lockOnBlur) {
        lockStore.lock();
      }
    }

    window.addEventListener("mousemove", resetIdle);
    window.addEventListener("keydown", resetIdle);
    window.addEventListener("click", resetIdle);
    window.addEventListener("blur", onBlur);
    document.addEventListener("visibilitychange", onVisibilityChange);
    resetIdle();

    return () => {
      if (idleTimer) clearTimeout(idleTimer);
      window.removeEventListener("mousemove", resetIdle);
      window.removeEventListener("keydown", resetIdle);
      window.removeEventListener("click", resetIdle);
      window.removeEventListener("blur", onBlur);
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  });

  function toggleNav(): void {
    navCollapsed = !navCollapsed;
    if (typeof localStorage !== "undefined") {
      localStorage.setItem(NAV_COLLAPSE_KEY, navCollapsed ? "1" : "0");
    }
  }

  $effect(() => {
    if (typeof window === "undefined") return;
    function onWindowError(event: ErrorEvent): void {
      errorStore.reportError(event.error ?? event.message);
    }
    function onUnhandledRejection(event: PromiseRejectionEvent): void {
      errorStore.reportError(event.reason);
    }
    window.addEventListener("error", onWindowError);
    window.addEventListener("unhandledrejection", onUnhandledRejection);
    return () => {
      window.removeEventListener("error", onWindowError);
      window.removeEventListener("unhandledrejection", onUnhandledRejection);
    };
  });
</script>

<div class="shell" class:shell--nav-collapsed={navCollapsed}>
  <aside class="sidebar">
    <div class="sidebar__head">
      <div class="sidebar__brand">av&#8209;sort</div>
      <button
        class="sidebar__toggle"
        type="button"
        onclick={toggleNav}
        aria-label={navCollapsed ? "사이드바 펼치기" : "사이드바 접기"}
        title={navCollapsed ? "사이드바 펼치기" : "사이드바 접기"}
      >
        {navCollapsed ? "»" : "«"}
      </button>
    </div>
    <nav class="sidebar__nav">
      {#each nav as item (item.href)}
        <a
          class="sidebar__link"
          class:sidebar__link--active={page.url.pathname === item.href}
          href={item.href}
          title={item.label}
          aria-label={item.label}
        >
          <Icon name={item.icon} class="sidebar__icon" />
          <span class="sidebar__label">{item.label}</span>
        </a>
      {/each}
    </nav>
    <div class="sidebar__status" data-tooltip={statusLabel(ping)}>
      <span
        class="status-dot {statusClass(ping)}"
        role="img"
        aria-label={statusLabel(ping)}
      ></span>
      {#if ping.ok}
        <span class="sidebar__status-version">v{ping.protocolVersion}</span>
      {/if}
    </div>
  </aside>

  <main class="shell__main">
    <svelte:boundary onerror={(error) => errorStore.reportError(error)}>
      {@render children()}
      {#snippet failed(_error, reset)}
        
        <div class="boundary-fallback" role="alert">
          <p class="boundary-fallback__msg">이 화면을 표시하는 중 오류가 발생했습니다.</p>
          <button class="btn btn--primary" type="button" onclick={reset}>
            다시 시도
          </button>
        </div>
      {/snippet}
    </svelte:boundary>
  </main>

  
  <ErrorToast />

  
  <ProtocolGate {ping} />

  
  <ScreenLock />
</div>

<style>
  .shell {
    display: grid;
    grid-template-columns: 200px 1fr;
    height: 100vh;
  }
  .sidebar {
    display: flex;
    flex-direction: column;
    gap: var(--space-md);
    padding: var(--space-sm) var(--space-xs);
    background: var(--color-canvas-elevated);
    border-right: 1px solid var(--color-hairline);
  }
  .sidebar__brand {
    font-size: var(--type-title-md-size);
    font-weight: var(--font-weight-strong);
    letter-spacing: 0.04em;
    padding: var(--space-xxs) var(--space-xs);
  }
  .sidebar__nav {
    display: flex;
    flex-direction: column;
    gap: var(--space-xxxs);
  }
  .sidebar__link {
    display: flex;
    align-items: center;
    gap: var(--space-xs);
    padding: var(--space-xxs) var(--space-xs);
    color: var(--color-body);
    font-size: var(--type-nav-link-size);
    font-weight: var(--type-nav-link-weight);
    letter-spacing: var(--type-nav-link-tracking);
    text-transform: var(--type-nav-link-transform);
    border-left: 2px solid transparent;
  }
  
  :global(.sidebar__icon) {
    display: none;
    font-size: 1.15rem;
    flex: none;
  }
  .sidebar__link:hover {
    color: var(--color-ink);
  }
  .sidebar__link--active {
    color: var(--color-ink);
    border-left-color: var(--color-primary);
  }
  .shell__main {
    overflow: hidden;
    min-width: 0;
  }
  


  .sidebar__status {
    position: relative;
    margin-top: auto;
    display: flex;
    align-items: center;
    gap: var(--space-xs);
    padding: var(--space-xxs) var(--space-xs);
    color: var(--color-body);
    font-size: var(--type-caption-size);
    font-variant-numeric: tabular-nums;
  }
  


  .sidebar__status::after {
    content: attr(data-tooltip);
    position: absolute;
    left: 100%;
    top: 50%;
    transform: translateY(-50%);
    margin-left: var(--space-xs);
    white-space: nowrap;
    padding: var(--space-xxs) var(--space-sm);
    background: var(--color-ink);
    color: var(--color-canvas);
    border-radius: var(--radius-sm, 4px);
    box-shadow: 0 2px 8px rgb(0 0 0 / 0.25);
    font-size: var(--type-caption-size);
    opacity: 0;
    visibility: hidden;
    pointer-events: none;
    transition: opacity 120ms ease;
    z-index: 20;
  }
  .sidebar__status:hover::after {
    opacity: 1;
    visibility: visible;
  }
  .status-dot {
    flex: none;
    width: 9px;
    height: 9px;
    border-radius: var(--radius-full);
    background: var(--color-semantic-warning);
  }
  .status-dot.status--ok {
    background: var(--color-semantic-success);
  }

  .sidebar__head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-xxs);
  }
  .sidebar__toggle {
    flex: none;
    padding: var(--space-xxxs) var(--space-xs);
    background: transparent;
    border: 1px solid var(--color-hairline);
    border-radius: var(--radius-sm, 4px);
    color: var(--color-muted);
    font-size: var(--type-body-md-size);
    line-height: 1;
    cursor: pointer;
  }
  .sidebar__toggle:hover {
    color: var(--color-ink);
  }

  

  .shell--nav-collapsed {
    grid-template-columns: 56px 1fr;
  }
  .shell--nav-collapsed .sidebar__brand,
  .shell--nav-collapsed .sidebar__label,
  .shell--nav-collapsed .sidebar__status-version {
    display: none;
  }
  .shell--nav-collapsed .sidebar__head {
    justify-content: center;
  }
  




  .shell--nav-collapsed .sidebar__status {
    justify-content: center;
    padding-left: 0;
    padding-right: 0;
  }
  .shell--nav-collapsed :global(.sidebar__icon) {
    display: inline-flex;
  }
  .shell--nav-collapsed .sidebar__link {
    justify-content: center;
  }

  .boundary-fallback {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-md);
    height: 100%;
    padding: var(--space-lg);
    text-align: center;
  }
  .boundary-fallback__msg {
    color: var(--color-semantic-error, #c0392b);
    font-size: var(--type-body-md-size);
  }
</style>
