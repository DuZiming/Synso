<script lang="ts">
  // Settings view (Task 19): the 设置 tab — language, launch-at-startup,
  // the global quick-search shortcut, and the index scope (exclusions +
  // volumes).
  //
  // Apply-loudly vs save-with-rescan, per the controller ruling:
  // - language (i18n's changeLang), autostart (autostart_set: OS
  //   registration + persist) and the shortcut (set_shortcut: live
  //   re-registration with rollback on conflict) take effect IMMEDIATELY;
  // - exclusions/volumes only shape the NEXT scan, so their 保存 shows
  //   the 「需要重新扫描后生效」 note plus a 重新扫描 button
  //   (start_full_scan).
  //
  // The volume picker and the exclusions textarea reuse the wizard's
  // markup and helpers (lib/settings.ts); scope saves carry the LIVE
  // language/autostart/shortcut values, never the stale mount snapshot,
  // so an apply-then-save sequence cannot revert an earlier change.
  import { onMount } from "svelte";
  import {
    changeLang,
    langSetting,
    t,
    type LangSetting,
  } from "../i18n";
  import {
    autostartSet,
    listVolumes,
    pickFolder,
    setShortcut,
    settingsGet,
    settingsSet,
    startFullScan,
    tierStats,
  } from "../lib/tauri";
  import {
    parseExclusions,
    customRootsOf,
    volumesChecked,
    volumesSavedWithCustoms,
  } from "../lib/settings";
  import { humanBytes } from "../lib/format";
  import type { Settings, TierStats, VolumeInfo } from "../lib/types";

  const langOptions = ["auto", "zh", "en"] as const;

  let loaded = $state(false);

  // Settings as persisted at mount; carries the fields this view edits
  // one-at-a-time back into every scope save (see the module comment).
  let base = $state<Settings | null>(null);
  /** The shortcut as last APPLIED (what the backend runs), which may lag
   * the input box while the user types a new combo. */
  let appliedShortcut = $state("");

  let volumes = $state<VolumeInfo[]>([]);
  /** Mount path -> checked (`[]` semantics: all fixed volumes). */
  let checked = $state<Record<string, boolean>>({});
  /** Arbitrary folder scan roots (dev/testing scope control) — engine
   * treats them exactly like volume roots. */
  let customRoots = $state<string[]>([]);
  let exclusionsText = $state("");

  // Language: changeLang persists + the tray re-labels (settings_set hook).
  // Autostart: optimistic switch, reverted on an OS-registration error.
  let autostart = $state(false);
  let autostartBusy = $state(false);
  let autostartError = $state<string | null>(null);

  // Shortcut: Apply re-registers live; errors keep the old combo working.
  let shortcutInput = $state("");
  let shortcutBusy = $state(false);
  let shortcutApplied = $state(false);
  let shortcutError = $state<string | null>(null);

  // Scope: 保存 persists; the rescan note stands until the rescan starts.
  let saveBusy = $state(false);
  let saveError = $state<string | null>(null);
  let savedPending = $state(false);
  let rescanStarted = $state(false);

  const checkedCount = $derived(volumes.filter((v) => checked[v.path]).length);
  const anyScope = $derived(checkedCount > 0 || customRoots.length > 0);

  // v1.2 memory policy: drafts over `base`, applied LIVE through the
  // normal settings_set flow (engine evicts to the new budgets at once).
  let memDraft = $state({
    hot: 64,
    staging: 32,
    ttl: 300,
    hits: 3,
    window: 10,
    threshold: 8,
    decay: 24,
    preload: true,
  });
  let memBusy = $state(false);
  let memSavedAt = $state<number | null>(null);
  /** Gauges, polled while the section renders. */
  let gauges = $state<TierStats | null>(null);
  let gaugeTimer: ReturnType<typeof setInterval> | undefined;

  const hotRate = $derived.by(() => {
    const g = gauges;
    if (!g || g.hot_hits + g.hot_misses === 0) return "—";
    return `${Math.round((g.hot_hits / (g.hot_hits + g.hot_misses)) * 100)}%`;
  });

  async function applyMemory(): Promise<void> {
    if (!base || memBusy) return;
    memBusy = true;
    const next: Settings = {
      ...base,
      mem_hot_budget_mb: memDraft.hot,
      mem_staging_budget_mb: memDraft.staging,
      mem_staging_ttl_secs: memDraft.ttl,
      mem_promote_hits: memDraft.hits,
      mem_promote_window_min: memDraft.window,
      mem_hot_threshold: memDraft.threshold,
      mem_decay_half_life_h: memDraft.decay,
      mem_preload_on_start: memDraft.preload,
    };
    try {
      await settingsSet(next);
      base = next;
      memSavedAt = Date.now();
    } catch {
      /* errors keep the previous policy; the toast pattern is overkill here */
    }
    memBusy = false;
  }

  function syncMemDraft(s: Settings): void {
    memDraft = {
      hot: s.mem_hot_budget_mb,
      staging: s.mem_staging_budget_mb,
      ttl: s.mem_staging_ttl_secs,
      hits: s.mem_promote_hits,
      window: s.mem_promote_window_min,
      threshold: s.mem_hot_threshold,
      decay: s.mem_decay_half_life_h,
      preload: s.mem_preload_on_start,
    };
  }

  function gb(bytes: number): string {
    return (bytes / 2 ** 30).toFixed(1);
  }

  async function toggleAutostart(next: boolean): Promise<void> {
    if (autostartBusy) return;
    autostart = next;
    autostartBusy = true;
    autostartError = null;
    try {
      await autostartSet(next);
    } catch (err) {
      autostart = !next;
      autostartError = String(err);
    }
    autostartBusy = false;
  }

  async function applyShortcut(): Promise<void> {
    const combo = shortcutInput.trim();
    if (shortcutBusy || combo === "") return;
    shortcutBusy = true;
    shortcutError = null;
    shortcutApplied = false;
    try {
      await setShortcut(combo);
      appliedShortcut = combo;
      shortcutApplied = true;
    } catch (err) {
      // The backend rolled back to the previous combo; say so verbatim.
      shortcutError = String(err);
    }
    shortcutBusy = false;
  }

  async function saveScope(): Promise<void> {
    const baseSettings = base;
    if (saveBusy || !baseSettings || !anyScope) return;
    saveBusy = true;
    saveError = null;
    try {
      await settingsSet({
        ...baseSettings,
        lang: $langSetting,
        autostart,
        shortcut: appliedShortcut,
        exclusions: parseExclusions(exclusionsText),
        volumes: volumesSavedWithCustoms(volumes, checked, customRoots),
      });
      base = await settingsGet();
      savedPending = true;
      rescanStarted = false;
    } catch (err) {
      saveError = String(err);
    }
    saveBusy = false;
  }

  function rescan(): void {
    savedPending = false;
    rescanStarted = true;
    void startFullScan();
  }

  async function addCustomRoot(): Promise<void> {
    const picked = await pickFolder();
    if (picked === null) return;
    if (!customRoots.includes(picked)) customRoots = [...customRoots, picked];
  }

  function removeCustomRoot(path: string): void {
    customRoots = customRoots.filter((p) => p !== path);
  }

  onMount(() => {
    void (async () => {
      const [settings, list] = await Promise.all([settingsGet(), listVolumes()]);
      base = settings;
      appliedShortcut = settings.shortcut;
      shortcutInput = settings.shortcut;
      autostart = settings.autostart;
      exclusionsText = settings.exclusions.join("\n");
      volumes = list;
      checked = volumesChecked(list, settings.volumes);
      customRoots = customRootsOf(settings.volumes, list);
      syncMemDraft(settings);
      loaded = true;
    })();

    // Tier gauges: poll while the section is mounted.
    void tierStats().then((g) => (gauges = g)).catch(() => {});
    gaugeTimer = setInterval(() => {
      void tierStats().then((g) => (gauges = g)).catch(() => {});
    }, 5000);
    return () => clearInterval(gaugeTimer);
  });
</script>

<main>
  {#if !loaded}
    <p class="hint">…</p>
  {:else}
    <section class="step">
      <h2>{$t("settings.langTitle")}</h2>
      <p class="hint">{$t("wizard.langHint")}</p>
      <select
        class="lang"
        aria-label={$t("a11y.langSelect")}
        value={$langSetting}
        onchange={(event) =>
          void changeLang(event.currentTarget.value as LangSetting)}
      >
        {#each langOptions as lang (lang)}
          <option value={lang}>{$t(`lang.${lang}`)}</option>
        {/each}
      </select>
    </section>

    <section class="step">
      <h2>{$t("settings.autostartTitle")}</h2>
      <label class="switch-row" for="settings-autostart-switch">
        <input
          id="settings-autostart-switch"
          type="checkbox"
          role="switch"
          checked={autostart}
          disabled={autostartBusy}
          onchange={(event) =>
            void toggleAutostart(event.currentTarget.checked)}
        />
        <span>{$t("wizard.autostartLabel")}</span>
      </label>
      <p class="hint">{$t("settings.autostartHint")}</p>
      {#if autostartError}
        <p class="error" role="alert">{autostartError}</p>
      {/if}
    </section>

    <section class="step">
      <h2>{$t("settings.shortcutTitle")}</h2>
      <p class="hint">{$t("settings.shortcutHint")}</p>
      <div class="shortcut-row">
        <input
          class="shortcut-input"
          type="text"
          spellcheck="false"
          aria-label={$t("a11y.shortcutInput")}
          bind:value={shortcutInput}
          onkeydown={(event) => {
            if (event.key === "Enter") void applyShortcut();
          }}
        />
        <button class="primary" onclick={() => void applyShortcut()} disabled={shortcutBusy}>
          {$t("settings.apply")}
        </button>
      </div>
      {#if shortcutApplied}
        <p class="ok" role="status">
          {$t("settings.applied")}: {appliedShortcut}
        </p>
      {/if}
      {#if shortcutError}
        <p class="error" role="alert">
          {$t("settings.shortcutFailed")}: {shortcutError}
        </p>
      {/if}
    </section>

    <section class="step">
      <h2>{$t("settings.indexTitle")}</h2>
      <p class="hint">{$t("wizard.volumesHint")}</p>
      <ul class="volumes" aria-label={$t("a11y.volumesList")}>
        {#each volumes as v (v.path)}
          <li>
            <label class="vol">
              <input type="checkbox" bind:checked={checked[v.path]} />
              <span class="vol-label">{v.label}</span>
              <span class="vol-path">{v.path}</span>
              <span class="vol-size">{gb(v.total_bytes)} GB</span>
            </label>
          </li>
        {/each}
      </ul>
      {#if checkedCount === 0 && customRoots.length === 0}
        <p class="error" role="alert">{$t("wizard.volumesEmpty")}</p>
      {/if}
      <p class="hint">{$t("settings.customHint")}</p>
      <ul class="volumes custom-roots" aria-label={$t("settings.customRoots")}>
        {#each customRoots as path (path)}
          <li>
            <label class="vol">
              <span class="vol-path">{path}</span>
              <button
                class="ghost"
                onclick={() => removeCustomRoot(path)}
                aria-label={$t("settings.removeRoot")}
              >
                {$t("settings.removeRoot")}
              </button>
            </label>
          </li>
        {/each}
      </ul>
      <button class="ghost" onclick={() => void addCustomRoot()}>
        {$t("settings.addFolder")}
      </button>
      <p class="hint">{$t("wizard.exclusionsHint")}</p>
      <p class="examples">
        <code>node_modules</code>
        <code>*.tmp</code>
        <code>.git</code>
      </p>
      <textarea
        class="exclusions"
        rows="6"
        spellcheck="false"
        aria-label={$t("a11y.exclusionsInput")}
        bind:value={exclusionsText}
      ></textarea>
      <footer class="wiz-nav">
        <button
          class="primary"
          onclick={() => void saveScope()}
          disabled={saveBusy || !anyScope}
        >
          {$t("settings.save")}
        </button>
      </footer>
      {#if saveError}
        <p class="error" role="alert">{saveError}</p>
      {/if}
      {#if savedPending}
        <p class="ok" role="status">
          {$t("settings.saved")}
          <button class="ghost" onclick={rescan}>
            {$t("dashboard.rescan")}
          </button>
        </p>
      {/if}
      {#if rescanStarted}
        <p class="hint" role="status">{$t("settings.rescanStarted")}</p>
      {/if}
    </section>

    <!-- v1.2: the memory policy — every knob live-appliable, with the
         tier gauges beside it for immediate feedback. -->
    <section class="step">
      <h2>{$t("settings.memTitle")}</h2>
      <p class="hint">{$t("settings.memHint")}</p>

      <div class="mem-grid">
        <label>
          {$t("settings.memHotBudget")}
          <input
            type="number" min="8" max="1024" step="8"
            bind:value={memDraft.hot}
          />
          MB
        </label>
        <label>
          {$t("settings.memStagingBudget")}
          <input
            type="number" min="8" max="512" step="8"
            bind:value={memDraft.staging}
          />
          MB
        </label>
        <label>
          {$t("settings.memTtl")}
          <input
            type="number" min="30" max="3600" step="30"
            bind:value={memDraft.ttl}
          />
          s
        </label>
        <label>
          {$t("settings.memPromoteHits")}
          <input
            type="number" min="1" max="10"
            bind:value={memDraft.hits}
          />
        </label>
        <label>
          {$t("settings.memPromoteWindow")}
          <input
            type="number" min="1" max="120"
            bind:value={memDraft.window}
          />
          min
        </label>
        <label>
          {$t("settings.memHotThreshold")}
          <input
            type="number" min="1" max="100"
            bind:value={memDraft.threshold}
          />
        </label>
        <label>
          {$t("settings.memDecay")}
          <input
            type="number" min="1" max="168"
            bind:value={memDraft.decay}
          />
          h
        </label>
        <label class="mem-toggle">
          <input type="checkbox" bind:checked={memDraft.preload} />
          {$t("settings.memPreload")}
        </label>
      </div>

      <p>
        <button class="primary" disabled={memBusy} onclick={() => void applyMemory()}>
          {$t("settings.memApply")}
        </button>
        {#if memSavedAt}
          <span class="ok">{$t("settings.memApplied")}</span>
        {/if}
      </p>

      {#if gauges}
        <div class="gauges" aria-label={$t("settings.memGauges")}>
          <span>
            {$t("settings.memHot")}
            {humanBytes(gauges.hot_bytes)} · {gauges.hot_entries}
            {$t("settings.memEntries")} · {$t("settings.memHitRate")} {hotRate}
          </span>
          <span>
            {$t("settings.memStaging")}
            {humanBytes(gauges.staging_bytes)} · {gauges.staging_entries}
            {$t("settings.memEntries")}
          </span>
          <span>
            {$t("settings.memEvictions")} {gauges.evictions} ·
            {$t("settings.memPromotions")} {gauges.promotions}
          </span>
        </div>
      {/if}
    </section>
  {/if}
</main>

<style>
  /* Local layout only; shared tokens (panel/border/step/volumes…) come
     from app.css so the view reads exactly like the wizard. */
  .shortcut-row {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 12px;
  }

  .shortcut-input {
    font: inherit;
    font-family: ui-monospace, Consolas, monospace;
    padding: 8px 12px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg);
    color: var(--text);
    width: 240px;
    max-width: 60vw;
  }

  .ok {
    color: var(--warm);
    margin: 0 0 8px;
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    overflow-wrap: anywhere;
  }

  .mem-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(210px, 1fr));
    gap: 10px 18px;
    margin: 12px 0;
  }

  .mem-grid label {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
  }

  .mem-grid input[type="number"] {
    width: 76px;
    font: inherit;
    padding: 5px 8px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg);
    color: var(--text);
  }

  .mem-toggle input {
    margin: 0;
  }

  .gauges {
    display: flex;
    flex-wrap: wrap;
    gap: 6px 18px;
    font-size: 12px;
    opacity: 0.8;
    margin-top: 10px;
  }
</style>
