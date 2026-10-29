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
    setShortcut,
    settingsGet,
    settingsSet,
    startFullScan,
  } from "../lib/tauri";
  import { parseExclusions, volumesChecked, volumesSaved } from "../lib/settings";
  import type { Settings, VolumeInfo } from "../lib/types";

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
    if (saveBusy || !baseSettings || checkedCount === 0) return;
    saveBusy = true;
    saveError = null;
    try {
      await settingsSet({
        ...baseSettings,
        lang: $langSetting,
        autostart,
        shortcut: appliedShortcut,
        exclusions: parseExclusions(exclusionsText),
        volumes: volumesSaved(volumes, checked),
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
      loaded = true;
    })();
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
      {#if checkedCount === 0}
        <p class="error" role="alert">{$t("wizard.volumesEmpty")}</p>
      {/if}
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
          disabled={saveBusy || checkedCount === 0}
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
</style>
