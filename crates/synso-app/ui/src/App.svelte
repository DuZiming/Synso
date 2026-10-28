<script lang="ts">
  import { onMount } from "svelte";
  import {
    initI18n,
    changeLang,
    t,
    langSetting,
    type LangSetting,
  } from "./i18n";
  import { quickInfo, quickShow, settingsGet } from "./lib/tauri";
  import Dashboard from "./dashboard/Dashboard.svelte";
  import Wizard from "./welcome/Wizard.svelte";

  // Routing (Task 14): on mount, settings_get decides wizard vs dashboard
  // (`first_run_done`). The wizard owns the first scan; its `onfinished`
  // (fired on scan-finished) flips this view to the dashboard, which then
  // mounts fresh and reads its own state (Task 16: tabs live there).

  let view = $state<"loading" | "wizard" | "dashboard">("loading");
  // Transient toast (e.g. global shortcut unavailable — Task 15).
  let toast = $state<string | null>(null);
  let toastTimer: ReturnType<typeof setTimeout> | undefined;

  const langOptions = ["auto", "zh", "en"] as const;

  function showToast(message: string): void {
    toast = message;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toast = null), 10_000);
  }

  function wizardFinished(): void {
    view = "dashboard";
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
      // Task 15: when the global shortcut could not be registered at
      // startup (taken or invalid), tell the user about the fallback
      // button — the quick window stays reachable regardless.
      try {
        const info = await quickInfo();
        if (!info.registered) showToast($t("toast.shortcut"));
      } catch {
        // quick_info itself unreachable — same degraded state as above.
      }
    })();

    return () => clearTimeout(toastTimer);
  });
</script>

<main>
  <header class="topbar">
    <h1>{$t("app.title")}</h1>
    {#if view === "dashboard"}
      <div class="top-actions">
        <!-- Task 15 fallback entry: opens the quick window even when the
             global shortcut is taken (its always-working path). -->
        <button class="ghost" onclick={() => void quickShow()}>
          {$t("dashboard.quickSearch")}
        </button>
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
      </div>
    {/if}
  </header>

  {#if view === "wizard"}
    <Wizard onfinished={wizardFinished} />
  {:else if view === "dashboard"}
    <Dashboard />
  {/if}

  {#if toast}
    <div class="toast" role="status">
      <span>{toast}</span>
      <button class="toast-x" aria-label={$t("a11y.dismiss")} onclick={() => (toast = null)}
        >×</button
      >
    </div>
  {/if}
</main>
