<script lang="ts">
  import { onMount } from "svelte";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import {
    changeLang,
    langSetting,
    t,
    type LangSetting,
  } from "../i18n";
  import {
    listVolumes,
    settingsGet,
    settingsSet,
    startFullScan,
  } from "../lib/tauri";
  import type { Settings, VolumeInfo } from "../lib/types";
  import { parseExclusions, volumesChecked, volumesSaved } from "../lib/settings";

  // First-run wizard (Task 14): language -> volumes -> exclusions ->
  // autostart notice, then 开始 persists the whole Settings (including
  // `first_run_done: true`) and starts the first scan; the scan-progress /
  // scan-finished / scan-failed events drive the progress view below.
  //
  // Every choice lives in wizard state until that final step — EXCEPT the
  // language, which reuses the dashboard toggle's exact semantics
  // (live switch + immediate persist through `changeLang`).
  //
  // The scan listeners attach at mount (not at the 开始 click) so a scan
  // already in flight — the dev-mode auto-scan — can never slip past
  // unseen; the handlers only act once the progress view is showing.

  let { onfinished }: { onfinished: () => void } = $props();

  const LAST_STEP = 3;
  const langOptions = ["auto", "zh", "en"] as const;
  const stepKeys = [
    "wizard.stepLang",
    "wizard.stepVolumes",
    "wizard.stepExclusions",
    "wizard.stepAutostart",
  ] as const;

  let step = $state(0); // 0..LAST_STEP while choosing
  let phase = $state<"steps" | "scanning">("steps");
  let loaded = $state(false);

  /** Settings as persisted at mount; carries fields the wizard does not
   * edit (e.g. `shortcut`) back into the final save untouched. */
  let base = $state<Settings | null>(null);
  let volumes = $state<VolumeInfo[]>([]);
  /** Mount path -> checked (all on by default; `volumes=[]` semantics). */
  let checked = $state<Record<string, boolean>>({});
  let exclusionsText = $state("");
  let autostart = $state(true);

  let progressDone = $state(0);
  let scanError = $state<string | null>(null);
  let beginning = $state(false);

  const checkedCount = $derived(volumes.filter((v) => checked[v.path]).length);

  function gb(bytes: number): string {
    return (bytes / 2 ** 30).toFixed(1);
  }

  function begin(): void {
    const baseSettings = base;
    if (phase !== "steps" || beginning || !baseSettings || checkedCount === 0)
      return;
    beginning = true;
    // Flip first, save second: an in-flight scan finishing while the save
    // round-trips still lands in the progress view instead of vanishing.
    phase = "scanning";
    scanError = null;
    void (async () => {
      try {
        await settingsSet({
          ...baseSettings,
          lang: $langSetting,
          autostart,
          exclusions: parseExclusions(exclusionsText),
          volumes: volumesSaved(volumes, checked),
          first_run_done: true,
        });
      } catch (err) {
        // The scan still runs, but the failure is surfaced in the progress
        // view (and the flag not being persisted means the wizard comes
        // back on the next launch — the honest outcome of a broken store).
        scanError = String(err);
      }
      beginning = false;
      void startFullScan();
    })();
  }

  function retry(): void {
    if (phase !== "scanning") return;
    scanError = null;
    void startFullScan();
  }

  onMount(() => {
    void (async () => {
      const [settings, list] = await Promise.all([settingsGet(), listVolumes()]);
      base = settings;
      autostart = settings.autostart;
      exclusionsText = settings.exclusions.join("\n");
      volumes = list;
      checked = volumesChecked(list, settings.volumes);
      loaded = true;
    })();

    const unlisteners: UnlistenFn[] = [];
    const on = <P,>(name: string, handler: (payload: P) => void): void => {
      void listen<P>(name, (event) => handler(event.payload)).then((un) =>
        unlisteners.push(un),
      );
    };
    on("scan-progress", (payload: { done: number }) => {
      if (phase === "scanning") progressDone = payload.done;
    });
    on("scan-finished", () => {
      if (phase === "scanning") onfinished();
    });
    on("scan-failed", (payload: { message: string }) => {
      if (phase === "scanning") scanError = payload.message;
    });

    return () => {
      for (const un of unlisteners) un();
    };
  });
</script>

<main>
  <section class="wizard">
    <header class="wiz-head">
      <h1>{$t("wizard.title")}</h1>
      <p class="hint">{$t("wizard.subtitle")}</p>
    </header>

    {#if phase === "steps"}
      <ol class="steps">
        {#each [0, 1, 2, 3] as i (i)}
          <li
            class:current={i === step}
            class:done={i < step}
            aria-current={i === step ? "step" : undefined}
          >
            {$t(stepKeys[i])}
          </li>
        {/each}
      </ol>
    {/if}

    {#if !loaded}
      <p class="hint">…</p>
    {:else if phase === "steps"}
      {#if step === 0}
        <section class="step">
          <h2>{$t("wizard.stepLang")}</h2>
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
      {:else if step === 1}
        <section class="step">
          <h2>{$t("wizard.stepVolumes")}</h2>
          <p class="hint">{$t("wizard.volumesHint")}</p>
          <ul class="volumes">
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
        </section>
      {:else if step === 2}
        <section class="step">
          <h2>{$t("wizard.stepExclusions")}</h2>
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
            bind:value={exclusionsText}
          ></textarea>
        </section>
      {:else}
        <section class="step">
          <h2>{$t("wizard.stepAutostart")}</h2>
          <label class="switch-row" for="autostart-switch">
            <input
              id="autostart-switch"
              type="checkbox"
              role="switch"
              bind:checked={autostart}
            />
            <span>{$t("wizard.autostartLabel")}</span>
          </label>
          <p class="hint">{$t("wizard.autostartHint")}</p>
        </section>
      {/if}

      <footer class="wiz-nav">
        {#if step > 0}
          <button class="ghost" onclick={() => (step -= 1)}>
            {$t("wizard.back")}
          </button>
        {/if}
        {#if step < LAST_STEP}
          <button class="primary" onclick={() => (step += 1)}>
            {$t("wizard.next")}
          </button>
        {:else}
          <button class="primary" onclick={begin} disabled={checkedCount === 0}>
            {$t("wizard.begin")}
          </button>
        {/if}
      </footer>
    {:else}
      <section class="step wiz-progress">
        <h2>{$t("wizard.scanning")}</h2>
        <div class="scanbar">
          <progress aria-label={$t("a11y.scanProgress")} class="bar"></progress>
          <span class="count">{progressDone.toLocaleString()}</span>
        </div>
        {#if scanError}
          <p class="error" role="alert">
            {$t("dashboard.scanFailed")}: {scanError}
          </p>
          <button class="primary" onclick={retry}>{$t("wizard.retry")}</button>
        {/if}
      </section>
    {/if}
  </section>
</main>
