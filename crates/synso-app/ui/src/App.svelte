<script lang="ts">
  import { onMount } from "svelte";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import {
    initI18n,
    changeLang,
    t,
    langSetting,
    type LangSetting,
  } from "./i18n";
  import { engineStats, settingsGet, startFullScan } from "./lib/tauri";
  import type { EngineStats } from "./lib/types";
  import Wizard from "./welcome/Wizard.svelte";

  // Routing (Task 14): on mount, settings_get decides wizard vs dashboard
  // (`first_run_done`). The wizard owns the first scan; its `onfinished`
  // (fired on scan-finished) flips this view to the dashboard. The scan /
  // fs-changed listeners below only drive dashboard state — while the
  // wizard shows, the events belong to its own progress view.

  let view = $state<"loading" | "wizard" | "dashboard">("loading");
  let stats = $state<EngineStats | null>(null);
  let scanning = $state(false);
  let scanError = $state<string | null>(null);
  let progressDone = $state(0);

  const langOptions = ["auto", "zh", "en"] as const;

  async function refresh(): Promise<void> {
    stats = await engineStats();
  }

  function scan(): void {
    scanError = null;
    scanning = true;
    void startFullScan();
  }

  function wizardFinished(): void {
    view = "dashboard";
    scanning = false;
    scanError = null;
    void refresh();
  }

  function mb(bytes: number): string {
    return (bytes / 1_048_576).toFixed(1);
  }

  onMount(() => {
    void (async () => {
      try {
        await initI18n();
        const settings = await settingsGet();
        view = settings.first_run_done ? "dashboard" : "wizard";
      } catch {
        // Engine unreachable: degrade to the dashboard shell (the tiles
        // show placeholders) instead of bricking the window on "loading".
        view = "dashboard";
      }
      if (view === "dashboard") void refresh();
    })();

    const unlisteners: UnlistenFn[] = [];
    const on = <P,>(name: string, handler: (payload: P) => void): void => {
      void listen<P>(name, (event) => handler(event.payload)).then((un) =>
        unlisteners.push(un),
      );
    };
    on("scan-progress", (payload: { done: number }) => {
      if (view === "dashboard") {
        scanning = true;
        progressDone = payload.done;
      }
    });
    on("scan-finished", () => {
      if (view === "dashboard") {
        scanning = false;
        progressDone = 0;
        void refresh();
      }
    });
    on("scan-failed", (payload: { message: string }) => {
      if (view === "dashboard") {
        scanning = false;
        progressDone = 0;
        scanError = payload.message;
      }
    });
    on("fs-changed", () => {
      if (view === "dashboard") void refresh();
    });

    return () => {
      for (const un of unlisteners) un();
    };
  });
</script>

<main>
  <header class="topbar">
    <h1>{$t("app.title")}</h1>
    {#if view === "dashboard"}
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
    {/if}
  </header>

  {#if view === "wizard"}
    <Wizard onfinished={wizardFinished} />
  {:else if view === "dashboard"}
    <section class="tiles">
      <div class="tile">
        <span class="value">{stats ? stats.files.toLocaleString() : "…"}</span>
        <span class="label">{$t("dashboard.files")}</span>
      </div>
      <div class="tile">
        <span class="value">{stats ? stats.dirs.toLocaleString() : "…"}</span>
        <span class="label">{$t("dashboard.dirs")}</span>
      </div>
      <div class="tile">
        <span
          class="value badge {stats?.warm ? 'warm' : 'cold'}"
          >{stats
            ? stats.warm
              ? $t("dashboard.warm")
              : $t("dashboard.cold")
            : "…"}</span
        >
        <span class="label">{$t("dashboard.index")}</span>
      </div>
      <div class="tile">
        <span class="value">
          {stats ? `${mb(stats.interned_bytes)} MB` : "…"}
        </span>
        <span class="label">{$t("dashboard.interned")}</span>
      </div>
    </section>

    <section class="scanbar">
      <button class="primary" onclick={scan} disabled={scanning}>
        {scanning
          ? $t("dashboard.scanning")
          : stats?.warm
            ? $t("dashboard.rescan")
            : $t("dashboard.scan")}
      </button>
      {#if scanning}
        <progress aria-label={$t("a11y.scanProgress")} class="bar"></progress>
        <span class="count">{progressDone.toLocaleString()}</span>
      {/if}
      {#if scanError}
        <p class="error" role="alert">{$t("dashboard.scanFailed")}: {scanError}</p>
      {/if}
    </section>
  {/if}
</main>
