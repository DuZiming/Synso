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
  import { engineStats, startFullScan } from "./lib/tauri";
  import type { EngineStats } from "./lib/types";

  // Dashboard skeleton (Task 13): stats tiles, scan button + progress,
  // language toggle. Real views arrive in Tasks 16-18.

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

  function mb(bytes: number): string {
    return (bytes / 1_048_576).toFixed(1);
  }

  onMount(() => {
    void initI18n();
    void refresh();

    const unlisteners: UnlistenFn[] = [];
    const on = <P,>(name: string, handler: (payload: P) => void): void => {
      void listen<P>(name, (event) => handler(event.payload)).then((un) =>
        unlisteners.push(un),
      );
    };
    on("scan-progress", (payload: { done: number }) => {
      scanning = true;
      progressDone = payload.done;
    });
    on("scan-finished", () => {
      scanning = false;
      progressDone = 0;
      void refresh();
    });
    on("scan-failed", (payload: { message: string }) => {
      scanning = false;
      progressDone = 0;
      scanError = payload.message;
    });
    on("fs-changed", () => {
      void refresh();
    });

    return () => {
      for (const un of unlisteners) un();
    };
  });
</script>

<main>
  <header class="topbar">
    <h1>{$t("app.title")}</h1>
    <select
      class="lang"
      aria-label="Language / 语言"
      value={$langSetting}
      onchange={(event) =>
        void changeLang(event.currentTarget.value as LangSetting)}
    >
      {#each langOptions as lang (lang)}
        <option value={lang}>{$t(`lang.${lang}`)}</option>
      {/each}
    </select>
  </header>

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
      <progress aria-label="scan progress" class="bar"></progress>
      <span class="count">{progressDone.toLocaleString()}</span>
    {/if}
    {#if scanError}
      <p class="error" role="alert">{$t("dashboard.scanFailed")}: {scanError}</p>
    {/if}
  </section>
</main>
