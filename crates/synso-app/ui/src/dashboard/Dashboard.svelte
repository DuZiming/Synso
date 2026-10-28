<script lang="ts">
  // Dashboard shell (Task 16): client-side tabs over the main window —
  // 概览 (index tiles + scan), 查重 (DedupView) and 火焰图 (FlameView,
  // Task 17); the 集合 (Task 18) tab is a disabled placeholder. No router
  // lib: tab state is one $state string, and tab content mounts on
  // demand, so a re-opened tab re-reads its own state.
  import { onMount } from "svelte";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { t } from "../i18n";
  import { humanBytes } from "../lib/format";
  import { engineStats, startFullScan } from "../lib/tauri";
  import type { EngineStats } from "../lib/types";
  import DedupView from "../dedup/DedupView.svelte";
  import FlameView from "../flame/FlameView.svelte";

  type TabId = "overview" | "dedup" | "flame";

  const tabs: { id: TabId; label: "tabs.overview" | "tabs.dedup" | "tabs.flame" }[] = [
    { id: "overview", label: "tabs.overview" },
    { id: "dedup", label: "tabs.dedup" },
    { id: "flame", label: "tabs.flame" },
  ];
  /// Task 18 owns this; rendered disabled until then.
  const soonTabs = ["tabs.collections"] as const;

  let tab = $state<TabId>("overview");
  let stats = $state<EngineStats | null>(null);
  let scanning = $state(false);
  let scanError = $state<string | null>(null);
  let progressDone = $state(0);

  async function refresh(): Promise<void> {
    stats = await engineStats();
  }

  function scan(): void {
    scanError = null;
    scanning = true;
    void startFullScan();
  }

  onMount(() => {
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
    on("fs-changed", () => void refresh());

    return () => {
      for (const un of unlisteners) un();
    };
  });
</script>

<div class="tabs" role="tablist" aria-label={$t("app.title")}>
  {#each tabs as tabDef (tabDef.id)}
    <button
      class="tab"
      role="tab"
      aria-selected={tab === tabDef.id}
      class:active={tab === tabDef.id}
      onclick={() => (tab = tabDef.id)}
    >
      {$t(tabDef.label)}
    </button>
  {/each}
  {#each soonTabs as key (key)}
    <button class="tab soon" role="tab" aria-selected="false" disabled>
      {$t(key)}<span class="soon-note">{$t("tabs.comingSoon")}</span>
    </button>
  {/each}
</div>

{#if tab === "overview"}
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
        {stats ? humanBytes(stats.interned_bytes) : "…"}
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
      <p class="error" role="alert">
        {$t("dashboard.scanFailed")}: {scanError}
      </p>
    {/if}
  </section>
{:else if tab === "flame"}
  <FlameView />
{:else}
  <DedupView onchanged={refresh} />
{/if}
