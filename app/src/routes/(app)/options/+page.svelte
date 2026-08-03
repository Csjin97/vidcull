<script lang="ts">
  import { onMount } from "svelte";
  import {
    loadSettings,
    saveSettings,
    defaultSettings,
    CPU_THROTTLE_OPTIONS,
    BEST_COPY_MODE_OPTIONS,
    type Settings,
    type CpuThrottle,
    type BestCopyMode,
    addScanFolder,
    removeScanFolder,
  } from "$lib/data/settings";
  import {
    pickFolder,
    rescanDirectory,
    forceRescanDirectory,
    setLogLevel,
    exportDiagnostics,
    openFolder,
  } from "$lib/ipc/tauri";
  import { lockStore } from "$lib/stores/lockStore.svelte";

  let folders = $state<string[]>([]);
  let newFolder = $state("");
  let excludePattern = $state("");
  let excludes = $state<string[]>([]);
  let runInBackground = $state(true);
  let autoIndex = $state(true);
  let startOnBoot = $state(false);
  let cpuThrottle = $state<CpuThrottle>("full");
  let bestCopyMode = $state<BestCopyMode>("archival");
  let workerCount = $state<number | null>(null);
  let cpuCores = $state(1);
  let partialClipsEnabled = $state(true);
  let indexingEnabled = $state(true);

  let loaded = $state(false);
  let loadFailed = $state(false);
  let saveState = $state<"idle" | "saving" | "saved" | "error">("idle");
  let saveError = $state("");

  let lockNewPw = $state("");
  let lockConfirmPw = $state("");
  let lockPwError = $state("");

  function apply(settings: Settings): void {
    folders = settings.scanFolders;
    excludes = settings.excludeRules;
    runInBackground = settings.backgroundEnabled;
    autoIndex = settings.autoIndex;
    startOnBoot = settings.runOnBoot;
    cpuThrottle = settings.cpuThrottle;
    bestCopyMode = settings.bestCopyMode;
    workerCount = settings.workerCount;
    cpuCores = settings.cpuCores;
    partialClipsEnabled = settings.partialClipsEnabled;
    indexingEnabled = settings.indexingEnabled;
  }

  function current(): Settings {
    return {
      scanFolders: folders,
      excludeRules: excludes,
      backgroundEnabled: runInBackground,
      autoIndex,
      runOnBoot: startOnBoot,
      cpuThrottle,
      bestCopyMode,
      workerCount,
      cpuCores,
      partialClipsEnabled,
      indexingEnabled,
    };
  }


  function autoWorkerCount(): number {
    return Math.max(1, Math.ceil(cpuCores / 2));
  }


  function onWorkerAutoToggle(e: Event): void {
    const auto = (e.currentTarget as HTMLInputElement).checked;
    workerCount = auto ? null : autoWorkerCount();
    void persist();
  }


  function onWorkerCountInput(e: Event): void {
    const n = Number((e.currentTarget as HTMLInputElement).value);
    workerCount = Math.min(cpuCores, Math.max(1, Math.round(n)));
    void persist();
  }

  onMount(() => {
    void (async () => {
      try {
        apply(await loadSettings());
        loadFailed = false;
      } catch (err) {
        loadFailed = true;
        saveState = "error";
        saveError = `설정을 불러오지 못했습니다: ${String(err)}`;
        apply(defaultSettings());
      } finally {
        loaded = true;
      }
    })();
  });


  async function persist(): Promise<void> {
    if (!loaded || loadFailed) return;
    saveState = "saving";
    saveError = "";
    try {
      apply(await saveSettings(current()));
      saveState = "saved";
    } catch (err) {
      saveState = "error";
      saveError = `저장 실패: ${String(err)}`;
    }
  }

  async function addFolderPath(path: string): Promise<void> {
    if (!path.trim()) return;
    saveState = "saving";
    saveError = "";
    try {
      apply(await addScanFolder(path));
      loadFailed = false;
      saveState = "saved";
    } catch (err) {
      saveState = "error";
      saveError = `저장 실패: ${String(err)}`;
    }
  }

  function addFolder(): void {
    void addFolderPath(newFolder);
    newFolder = "";
  }


  async function browseFolder(): Promise<void> {
    try {
      const picked = await pickFolder();
      if (picked) await addFolderPath(picked);
    } catch (err) {
      saveState = "error";
      saveError = `폴더 선택을 열지 못했습니다: ${String(err)}`;
    }
  }

  async function removeFolder(path: string): Promise<void> {
    saveState = "saving";
    saveError = "";
    try {
      apply(await removeScanFolder(path));
      loadFailed = false;
      saveState = "saved";
    } catch (err) {
      saveState = "error";
      saveError = `저장 실패: ${String(err)}`;
    }
  }

  // Rescan/force-rescan act per-folder on the daemon side (Action::Rescan /
  // Action::ForceRescan each take one path), but the UI only offers one
  // "전체" pair covering every configured folder — a per-folder button row
  // just duplicated the same two actions N times with no real benefit, since
  // rescanning a single folder in isolation is rarely what's actually wanted.
  async function rescanAllFolders(): Promise<void> {
    if (folders.length === 0) return;
    saveState = "saving";
    saveError = "";
    const failures: string[] = [];
    for (const path of folders) {
      try {
        await rescanDirectory(path);
      } catch (err) {
        failures.push(`${path}: ${String(err)}`);
      }
    }
    if (failures.length === 0) {
      saveState = "saved";
      saveError = `${folders.length}개 폴더 재스캔이 시작되었습니다.`;
    } else {
      saveState = "error";
      saveError = `일부 폴더 재스캔 시작 실패 — ${failures.join(" / ")}`;
    }
  }

  async function forceRescanAllFolders(): Promise<void> {
    if (folders.length === 0) return;
    const ok =
      typeof window !== "undefined" && typeof window.confirm === "function"
        ? window.confirm(
            `등록된 ${folders.length}개 폴더의 모든 영상을 다시 해시·디코드합니다 ` +
              `(캐시된 해시·지문 무시). 파일이 많으면 오래 걸릴 수 있습니다. 진행할까요?`,
          )
        : true;
    if (!ok) return;
    saveState = "saving";
    saveError = "";
    const failures: string[] = [];
    for (const path of folders) {
      try {
        await forceRescanDirectory(path);
      } catch (err) {
        failures.push(`${path}: ${String(err)}`);
      }
    }
    if (failures.length === 0) {
      saveState = "saved";
      saveError = `${folders.length}개 폴더 강제 재스캔이 시작되었습니다.`;
    } else {
      saveState = "error";
      saveError = `일부 폴더 강제 재스캔 시작 실패 — ${failures.join(" / ")}`;
    }
  }

  function addExclude(): void {
    const trimmed = excludePattern.trim();
    if (trimmed && !excludes.includes(trimmed)) {
      excludes = [...excludes, trimmed];
      void persist();
    }
    excludePattern = "";
  }

  function removeExclude(pattern: string): void {
    excludes = excludes.filter((e) => e !== pattern);
    void persist();
  }


  function onToggle(): void {
    void persist();
  }

  let logLevel = $state<"info" | "debug" | "trace" | "warn" | "error">("info");
  let lastExportDest = $state<string | null>(null);


  async function onLogLevelChange(): Promise<void> {
    saveState = "saving";
    saveError = "";
    try {
      const detail = await setLogLevel(logLevel);
      saveState = "saved";
      saveError = `로그 레벨이 적용되었습니다: ${detail}`;
    } catch (err) {
      saveState = "error";
      saveError = `로그 레벨 변경 실패: ${String(err)}`;
    }
  }


  async function exportDiagnosticBundle(): Promise<void> {
    saveState = "saving";
    saveError = "";
    try {
      const dest = await pickFolder();
      if (!dest) {
        saveState = "idle";
        return;
      }
      const detail = await exportDiagnostics(dest);
      lastExportDest = dest;
      saveState = "saved";
      saveError = `진단 로그를 내보냈습니다 (${dest}): ${detail}`;
    } catch (err) {
      saveState = "error";
      saveError = `진단 로그 내보내기 실패: ${String(err)}`;
    }
  }



  async function setLockPassword(): Promise<void> {
    lockPwError = "";
    if (!lockNewPw) {
      lockPwError = "비밀번호를 입력하세요.";
      return;
    }
    if (lockNewPw !== lockConfirmPw) {
      lockPwError = "비밀번호가 일치하지 않습니다.";
      return;
    }
    await lockStore.setPassword(lockNewPw);
    lockNewPw = "";
    lockConfirmPw = "";
    saveState = "saved";
    saveError = "화면 잠금 비밀번호가 설정되었습니다.";
  }

  function disableLock(): void {
    lockStore.disable();
    lockNewPw = "";
    lockConfirmPw = "";
    lockPwError = "";
    saveState = "saved";
    saveError = "화면 잠금이 해제되었습니다.";
  }

  function onIdleMinutesChange(e: Event): void {
    const v = Number((e.currentTarget as HTMLSelectElement).value);
    lockStore.setIdleMinutes(v);
  }

  function onLockOnBlurToggle(e: Event): void {
    lockStore.setLockOnBlur((e.currentTarget as HTMLInputElement).checked);
  }
</script>

<div class="options">
  <header>
    <p class="eyebrow">옵션</p>
    <h1 class="options__title">설정</h1>
  </header>

  <div class="options__actions">
    <span class="options__autosave">변경 사항은 자동으로 저장됩니다.</span>
    {#if saveState === "saving"}
      <span class="options__status" data-testid="save-status">저장 중…</span>
    {:else if saveState === "saved"}
      <span class="options__status options__status--ok" data-testid="save-status">
        저장됨
      </span>
    {:else if saveState === "error"}
      <span
        class="options__status options__status--err"
        data-testid="save-status"
      >
        {saveError}
      </span>
    {/if}
  </div>

  <section class="opt-card">
    <h2>탐색 폴더</h2>
    <div class="opt-row">
      <input
        class="input"
        type="text"
        bind:value={newFolder}
        placeholder="예: D:/videos"
        onkeydown={(e) => e.key === "Enter" && addFolder()}
      />
      <button class="btn btn--primary" type="button" onclick={browseFolder}>찾아보기…</button>
      <button class="btn btn--outline btn--add" type="button" onclick={addFolder}>추가</button>
    </div>
    {#if folders.length === 0}
      <p class="opt-empty">등록된 폴더가 없습니다. 탐색할 폴더를 추가하세요.</p>
    {:else}
      <ul class="opt-list">
        {#each folders as folder (folder)}
          <li>
            <span class="opt-folder-path" title={folder}>{folder}</span>
            <div class="opt-list__actions">
              <button class="btn btn--ghost" type="button" onclick={() => removeFolder(folder)}>삭제</button>
            </div>
          </li>
        {/each}
      </ul>
      <div class="opt-row opt-row--rescan-all">
        <button class="btn btn--outline" type="button" onclick={rescanAllFolders}>전체 재스캔</button>
        <button
          class="btn btn--outline"
          type="button"
          title="캐시된 해시·지문을 무시하고 등록된 모든 폴더의 파일을 다시 계산"
          onclick={forceRescanAllFolders}
        >전체 강제 재스캔</button>
      </div>
    {/if}
  </section>

  <section class="opt-card">
    <h2>백그라운드 동작</h2>
    <label class="opt-toggle">
      <input type="checkbox" bind:checked={runInBackground} onchange={onToggle} />
      UI를 닫아도 백그라운드 앱으로 실행 (트레이 최소화)
    </label>
    <label class="opt-toggle">
      <input
        type="checkbox"
        bind:checked={autoIndex}
        onchange={onToggle}
        disabled={!runInBackground}
      />
      백그라운드 동작 시 자동 인덱싱
    </label>
    <label class="opt-toggle">
      <input type="checkbox" bind:checked={startOnBoot} onchange={onToggle} />
      시스템 시작 시 자동 실행
    </label>
  </section>

  <section class="opt-card">
    <h2>CPU 사용</h2>
    <p class="opt-empty">
      백그라운드 인덱싱이 CPU를 얼마나 사용할지 정합니다. 더 아끼면 인덱싱이
      느려지는 대신 다른 작업에 CPU를 양보합니다. 변경 즉시 적용됩니다.
    </p>
    <label class="opt-field">
      <span>인덱싱 속도</span>
      <select
        class="input"
        data-testid="cpu-throttle"
        bind:value={cpuThrottle}
        onchange={onToggle}
      >
        {#each CPU_THROTTLE_OPTIONS as opt (opt.value)}
          <option value={opt.value}>{opt.label} — {opt.hint}</option>
        {/each}
      </select>
    </label>

    
    <label class="opt-toggle">
      <input
        type="checkbox"
        data-testid="worker-auto"
        checked={workerCount === null}
        onchange={onWorkerAutoToggle}
      />
      인덱싱 워커 수 자동 (코어 절반 사용 — 권장)
    </label>
    {#if workerCount !== null}
      <label class="opt-field">
        <span>동시 워커 수: {workerCount} / {cpuCores}</span>
        <input
          class="input"
          type="range"
          data-testid="worker-count"
          min="1"
          max={cpuCores}
          step="1"
          value={workerCount}
          oninput={onWorkerCountInput}
        />
      </label>
      <p class="opt-empty">
        워커를 늘리면 유휴 상태에서 인덱싱이 빨라지지만, 물리 코어 수를 넘어서면
        이득이 줄고 노트북에서는 발열·배터리 소모가 커집니다. 사용 중일 때는
        설정과 무관하게 워커가 1개로 자동 감속되어 다른 작업을 방해하지 않습니다.
      </p>
    {/if}
  </section>

  <section class="opt-card">
    <h2>최적 사본 선정 정책</h2>
    <p class="opt-empty">
      중복 비디오 그룹에서 어떤 파일을 '최적 사본(Best Copy)'으로 남기고 보존할지 결정합니다.
      변경 즉시 적용되며, 데몬이 즉시 새로운 정책을 기반으로 최적 사본을 재산출합니다.
    </p>
    <label class="opt-field">
      <span>보존 정책</span>
      <select
        class="input"
        data-testid="best-copy-mode"
        bind:value={bestCopyMode}
        onchange={onToggle}
      >
        {#each BEST_COPY_MODE_OPTIONS as opt (opt.value)}
          <option value={opt.value}>{opt.label} — {opt.hint}</option>
        {/each}
      </select>
    </label>
  </section>

  <section class="opt-card">
    <h2>중복 검출 고급</h2>
    <p class="opt-empty">
      리프레임 부분클립 검출은 한 영상의 일부 구간이 다른 영상에 재편집되어 들어간
      경우까지 찾아냅니다. 가장 무거운 분석 패스이므로 기본적으로 꺼져 있으며, 켜면
      다음 데몬 시작 시부터 백그라운드에서 적용됩니다.
    </p>
    <label class="opt-toggle">
      <input
        type="checkbox"
        data-testid="partial-clips"
        bind:checked={partialClipsEnabled}
        onchange={onToggle}
      />
      리프레임 부분클립 검출
    </label>
  </section>

  <section class="opt-card">
    <h2>폴더명 제외 규칙</h2>
    <p class="opt-empty">
      이름이 일치하는 폴더(와 그 하위 전체)를 인덱싱에서 제외합니다(대소문자 무시).
      $RECYCLE.BIN·System Volume Information 등 Windows 시스템 폴더는 기본으로
      제외되어 있으며, 여기서 직접 추가·삭제할 수 있습니다.
    </p>
    <div class="opt-row">
      <input
        class="input"
        type="text"
        bind:value={excludePattern}
        placeholder="예: .trash, node_modules"
        onkeydown={(e) => e.key === "Enter" && addExclude()}
      />
      <button class="btn btn--outline btn--add" type="button" onclick={addExclude}>추가</button>
    </div>
    {#if excludes.length === 0}
      <p class="opt-empty">제외 규칙이 없습니다. 제외할 폴더명을 추가하세요.</p>
    {:else}
      <ul class="opt-list">
        {#each excludes as pattern (pattern)}
          <li>
            <span class="opt-folder-path" title={pattern}>{pattern}</span>
            <div class="opt-list__actions">
              <button class="btn btn--ghost" type="button" onclick={() => removeExclude(pattern)}>삭제</button>
            </div>
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <section class="opt-card">
    <h2>화면 잠금</h2>
    <p class="opt-empty">
      앱을 자리를 비울 때 화면을 가려 주변 시선으로부터 내용을 숨깁니다.
      비밀번호를 먼저 설정해야 잠금을 켤 수 있습니다.
    </p>

    
    <label class="opt-toggle" class:opt-toggle--disabled={!lockStore.hasPassword}>
      <input
        type="checkbox"
        data-testid="lock-enabled"
        checked={lockStore.enabled}
        disabled={!lockStore.hasPassword}
        onchange={(e) => {
          if ((e.currentTarget as HTMLInputElement).checked) {
            lockStore.lock();
          } else {
            lockStore.disable();
          }
        }}
      />
      화면 잠금 사용 {lockStore.enabled ? "(켜짐)" : "(꺼짐)"}
      {#if !lockStore.hasPassword}
        <span class="opt-hint">— 비밀번호를 먼저 설정하세요</span>
      {/if}
    </label>

    
    <div class="opt-field">
      <span>비밀번호 설정 / 변경</span>
      <input
        class="input lock-input"
        type="password"
        placeholder="새 비밀번호"
        bind:value={lockNewPw}
        autocomplete="new-password"
        onkeydown={(e) => e.key === "Enter" && void setLockPassword()}
      />
      <input
        class="input lock-input"
        type="password"
        placeholder="비밀번호 확인"
        bind:value={lockConfirmPw}
        autocomplete="new-password"
        onkeydown={(e) => e.key === "Enter" && void setLockPassword()}
      />
      {#if lockPwError}
        <p class="opt-error" role="alert">{lockPwError}</p>
      {/if}
      <div class="opt-row">
        <button
          class="btn btn--primary"
          type="button"
          onclick={() => void setLockPassword()}
        >
          비밀번호 설정
        </button>
        {#if lockStore.hasPassword}
          <button class="btn btn--ghost" type="button" onclick={disableLock}>
            비밀번호 해제 (잠금 끄기)
          </button>
        {/if}
      </div>
    </div>

    
    <label class="opt-field">
      <span>유휴 자동 잠금</span>
      <select
        class="input"
        data-testid="lock-idle-minutes"
        value={lockStore.idleMinutes}
        onchange={onIdleMinutesChange}
        disabled={!lockStore.enabled}
      >
        <option value={0}>끔</option>
        <option value={1}>1분</option>
        <option value={5}>5분</option>
        <option value={15}>15분</option>
      </select>
    </label>

    
    <label class="opt-toggle" class:opt-toggle--disabled={!lockStore.enabled}>
      <input
        type="checkbox"
        data-testid="lock-on-blur"
        checked={lockStore.lockOnBlur}
        disabled={!lockStore.enabled}
        onchange={onLockOnBlurToggle}
      />
      창이 비활성화될 때 자동 잠금
    </label>

    
    {#if lockStore.enabled}
      <div class="opt-row">
        <button
          class="btn btn--outline"
          type="button"
          data-testid="lock-now"
          onclick={() => lockStore.lock()}
        >
          지금 잠금
        </button>
      </div>
    {/if}

    <p class="opt-disclaimer">
      주변 시선 가림용입니다. 디스크 접근으로부터 데이터를 보호하지 않습니다(그건 OS 디스크 암호화).
    </p>
  </section>

  <section class="opt-card">
    <h2>진단 / 로그</h2>
    <p class="opt-empty">
      버그·느려짐이 있으면 아래 <strong>진단 로그 내보내기</strong>로 로그를 모아
      개발자에게 전달하세요. 로그의 파일 경로는 개인정보 보호를 위해 익명 토큰으로
      가려져 그대로 공유해도 안전합니다. 첫 재현이라면 위 <strong>로그 상세도</strong>를
      <em>상세 (debug)</em>로 올린 뒤 재현하고 내보내면 진단에 도움이 됩니다.
    </p>
    <label class="opt-field">
      <span>로그 상세도 (데몬 재시작 없이 즉시 적용 · 재시작 시 기본값 복귀)</span>
      <select
        class="input"
        data-testid="log-level"
        bind:value={logLevel}
        onchange={onLogLevelChange}
      >
        <option value="info">기본 (info)</option>
        <option value="debug">상세 (debug) — 첫 재현 시 권장</option>
        <option value="trace">매우 상세 (trace)</option>
        <option value="warn">경고만 (warn)</option>
        <option value="error">오류만 (error)</option>
      </select>
    </label>
    <div class="opt-row">
      <button
        class="btn btn--primary"
        type="button"
        data-testid="export-diagnostics"
        onclick={exportDiagnosticBundle}
      >
        진단 로그 내보내기…
      </button>
      {#if lastExportDest}
        <button
          class="btn btn--outline"
          type="button"
          data-testid="open-diagnostics-folder"
          onclick={() => openFolder(lastExportDest!)}
        >
          결과 폴더 열기
        </button>
      {/if}
    </div>
  </section>
</div>

<style>
  .options {
    display: flex;
    flex-direction: column;
    gap: var(--space-md);
    height: 100%;
    overflow-y: auto;
    padding: var(--space-md) var(--space-lg);
  }
  .options__title {
    font-size: var(--type-display-lg-size);
    letter-spacing: var(--type-display-lg-tracking);
  }
  .options__actions {
    display: flex;
    align-items: center;
    gap: var(--space-sm);
  }
  .options__autosave {
    font-size: var(--type-body-sm-size);
    color: var(--color-muted);
  }
  .options__status {
    font-size: var(--type-body-sm-size);
  }
  .options__status--ok {
    color: var(--color-semantic-success, var(--color-body));
  }
  .options__status--err {
    color: var(--color-danger, #c81d25);
  }
  .opt-card {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
    padding: var(--space-sm);
    background: var(--color-surface-card);
    border: 1px solid var(--color-hairline);
  }
  .opt-card h2 {
    font-size: var(--type-display-md-size);
  }
  .opt-row {
    display: flex;
    gap: var(--space-xs);
  }
  .opt-row--rescan-all {
    margin-top: var(--space-xxs);
  }
  .opt-row button {
    padding: 0 var(--space-md);
    min-width: 150px;
  }
  .opt-row button.btn--add {
    min-width: 80px;
    padding: 0 var(--space-xs);
  }
  .opt-empty {
    margin: 0;
    color: var(--color-muted);
    font-size: var(--type-body-sm-size);
  }
  .opt-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-xxxs);
  }
  .opt-list li {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: var(--space-xxs) var(--space-xs);
    background: var(--color-canvas);
    border: 1px solid var(--color-hairline);
    font-size: var(--type-body-sm-size);
  }
  .opt-list__actions {
    display: flex;
    gap: var(--space-xxs);
    flex-shrink: 0;
  }
  


  .opt-folder-path {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .opt-toggle {
    display: flex;
    align-items: center;
    gap: var(--space-xxs);
    font-size: var(--type-body-md-size);
  }
  .opt-field {
    display: flex;
    flex-direction: column;
    gap: var(--space-xxs);
    font-size: var(--type-body-md-size);
  }
  .opt-field select {
    max-width: 360px;
  }
  .opt-toggle--disabled {
    opacity: 0.5;
  }
  .opt-hint {
    color: var(--color-muted);
    font-size: var(--type-caption-size);
  }
  .lock-input {
    max-width: 320px;
  }
  .opt-error {
    margin: 0;
    color: var(--color-semantic-warning);
    font-size: var(--type-body-sm-size);
  }
  .opt-disclaimer {
    margin: var(--space-xxs) 0 0;
    color: var(--color-muted);
    font-size: var(--type-caption-size);
    line-height: 1.5;
  }
</style>
