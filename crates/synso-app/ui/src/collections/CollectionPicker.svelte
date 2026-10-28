<script lang="ts">
  // Collection picker (Task 18): the small dialog BOTH add-to-collection
  // paths share — the quick window's multi-select 加入集合 button and the
  // flame chart's right-click menu item. Lists the collections; picking
  // one calls `onpick`; the inline 新建 row creates AND picks in one step
  // (the created collection is the natural target). The CALLER performs
  // the actual `coll_add` — the picker only resolves WHICH collection the
  // ids go to, so it stays dumb and reusable.
  //
  // Esc closes here (FlameView hosts this over a normal window); the
  // quick window, whose own Esc hides the whole window, checks its
  // picker-open flag first and swallows the key while this is up.
  import { onMount } from "svelte";
  import { t } from "../i18n";
  import { collCreate, collList } from "../lib/tauri";
  import type { Collection } from "../lib/types";

  let {
    onpick,
    onclose,
  }: {
    onpick: (collection: Collection) => void;
    onclose: () => void;
  } = $props();

  let colls = $state<Collection[] | null>(null);
  let loadError = $state<string | null>(null);
  let newName = $state("");
  let creating = $state(false);
  let createError = $state<string | null>(null);
  let nameEl: HTMLInputElement | undefined = $state();

  async function refresh(): Promise<void> {
    try {
      colls = await collList();
      loadError = null;
    } catch (err) {
      loadError = String(err);
    }
  }

  async function createAndPick(): Promise<void> {
    const name = newName.trim();
    if (!name || creating) return;
    creating = true;
    createError = null;
    try {
      onpick(await collCreate(name));
    } catch (err) {
      createError = String(err);
    } finally {
      creating = false;
    }
  }

  onMount(() => {
    void refresh();
    nameEl?.focus();
  });
</script>

<svelte:window
  onkeydown={(event) => {
    if (event.key === "Escape") onclose();
  }}
/>

<div class="picker-overlay">
  <div
    class="picker"
    role="dialog"
    aria-modal="true"
    aria-label={$t("a11y.pickerDialog")}
  >
    <h3>{$t("collections.addTitle")}</h3>

    {#if colls === null}
      <p class="muted">{loadError ?? "…"}</p>
    {:else if colls.length === 0}
      <p class="muted">{$t("collections.pickerEmpty")}</p>
    {:else}
      <ul class="picker-list">
        {#each colls as coll (coll.id)}
          <li>
            <button class="picker-item" onclick={() => onpick(coll)}>
              {coll.name}
            </button>
          </li>
        {/each}
      </ul>
    {/if}

    <div class="picker-new">
      <input
        bind:this={nameEl}
        bind:value={newName}
        placeholder={$t("collections.createPlaceholder")}
        aria-label={$t("collections.createPlaceholder")}
        spellcheck="false"
        autocomplete="off"
        oninput={() => (createError = null)}
        onkeydown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            void createAndPick();
          }
        }}
      />
      <button
        class="ghost"
        disabled={!newName.trim() || creating}
        onclick={() => void createAndPick()}
      >
        {$t("collections.pickerCreate")}
      </button>
    </div>
    {#if createError}
      <p class="error" role="alert">{createError}</p>
    {/if}

    <footer class="picker-actions">
      <button class="ghost" onclick={onclose}>
        {$t("collections.confirmCancel")}
      </button>
    </footer>
  </div>
</div>

<style>
  .picker-overlay {
    position: fixed;
    inset: 0;
    background: rgb(0 0 0 / 0.4);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 40;
  }

  .picker {
    background: var(--panel);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 16px 18px;
    width: min(320px, calc(100vw - 48px));
    max-height: min(420px, calc(100vh - 64px));
    display: flex;
    flex-direction: column;
    gap: 10px;
    box-shadow: 0 10px 40px rgb(0 0 0 / 0.25);
  }

  h3 {
    margin: 0;
    font-size: 16px;
  }

  .muted {
    color: var(--muted);
    margin: 0;
  }

  .picker-list {
    list-style: none;
    padding: 0;
    margin: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .picker-item {
    font: inherit;
    text-align: left;
    color: var(--text);
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 8px 12px;
    cursor: pointer;
    overflow-wrap: anywhere;
  }

  .picker-item:hover {
    border-color: var(--accent);
    color: var(--accent);
  }

  .picker-new {
    display: flex;
    gap: 8px;
  }

  .picker-new input {
    font: inherit;
    flex: 1;
    min-width: 0;
    color: var(--text);
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 8px 12px;
  }

  .error {
    color: var(--error);
    margin: 0;
    overflow-wrap: anywhere;
  }

  .picker-actions {
    display: flex;
    justify-content: flex-end;
  }
</style>
