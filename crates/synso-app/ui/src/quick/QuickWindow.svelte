<script lang="ts">
  // Quick search window (Task 15): one input, one windowed result list.
  // Search runs 30ms-debounced through the `search` command; the parsed
  // modifier chips come from `parse_preview` (the engine's own parser, so
  // the chips always match what was searched). Enter opens the selection
  // with its default app, Ctrl+Enter reveals it in Explorer, Esc (or a
  // focus loss, handled Rust-side) hides the window; re-showing keeps the
  // query text, selects it, and clears the result selection.
  import { onMount } from "svelte";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { initI18n, t } from "../i18n";
  import {
    engineStats,
    openPath,
    parsePreview,
    quickHide,
    revealPath,
    search,
  } from "../lib/tauri";
  import type { Hit, ParsedQuery } from "../lib/types";

  /// Fixed row height — the pixel grid the windowing slices on (and the
  /// rows' CSS height; both sides must agree).
  const ROW_H = 44;
  /// Search page size (the ranked head is all a popup shows).
  const LIMIT = 100;
  /// The brief's input debounce.
  const DEBOUNCE_MS = 30;
  /// Rows rendered above/below the viewport so scrolling stays smooth.
  const OVERSCAN = 5;

  let query = $state("");
  let hits = $state<Hit[]>([]);
  let total = $state(0);
  let parsed = $state<ParsedQuery | null>(null);
  /// Index into `hits`, -1 = nothing selected.
  let selected = $state(-1);
  /// True when a non-empty query came back empty on a cold index.
  let warming = $state(false);
  /// Last open/reveal failure (stays visible so the user can read it).
  let openError = $state<string | null>(null);

  let listEl: HTMLDivElement | undefined = $state();
  let inputEl: HTMLInputElement | undefined = $state();
  let scrollTop = $state(0);
  let viewportH = $state(0);

  // The visible window of rows: slice the hit list by scroll position
  // over the fixed row height (no virtual-scroll library), absolutely
  // positioned at `index * ROW_H` inside a spacer of the full height.
  const visible = $derived.by(() => {
    const start = Math.max(0, Math.floor(scrollTop / ROW_H) - OVERSCAN);
    const end = Math.min(
      hits.length,
      Math.ceil((scrollTop + viewportH) / ROW_H) + OVERSCAN,
    );
    const rows: { hit: Hit; index: number }[] = [];
    for (let i = start; i < end; i++) rows.push({ hit: hits[i], index: i });
    return rows;
  });

  // The modifiers the engine recognized, spelled back as chips.
  const chips = $derived.by(() => {
    const p = parsed;
    if (!p) return [] as string[];
    const out: string[] = [];
    if (p.ext) out.push(`ext:${p.ext}`);
    // min/max store inclusive bounds; `>`/`<` queries are strict, so the
    // humanized bound is one byte off the stored value.
    if (p.min_size !== null) out.push(`size:>${humanBytes(p.min_size - 1)}`);
    if (p.max_size !== null) out.push(`size:<${humanBytes(p.max_size + 1)}`);
    if (p.path_prefix) out.push(`path:${p.path_prefix}`);
    for (const name of p.collections) out.push(`coll:${name}`);
    return out;
  });

  let debounceTimer: ReturnType<typeof setTimeout> | undefined;
  /// Stale-response guard: only the newest run may touch the state.
  let runId = 0;

  function queueSearch(): void {
    clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => void runSearch().catch(() => {}), DEBOUNCE_MS);
  }

  // The window can be shown within the startup race (state managed after
  // the pages mount), and the IPC can fail transiently in either
  // direction — a popup just keeps its last state instead of erroring.
  async function runSearch(): Promise<void> {
    const q = query;
    const run = ++runId;
    const res = await search(q, 0, LIMIT);
    if (run !== runId) return;
    hits = res.hits;
    total = res.total;
    selected = -1;
    scrollTop = 0;
    if (listEl) listEl.scrollTop = 0;

    // A cold index legitimately returns nothing: say "warming up" instead
    // of "no results" (only worth an extra stats round-trip then).
    if (res.hits.length === 0 && q.trim() !== "") {
      try {
        const stats = await engineStats();
        if (run === runId) warming = !stats.warm;
      } catch {
        if (run === runId) warming = false;
      }
    } else if (run === runId) {
      warming = false;
    }

    try {
      const preview = await parsePreview(q);
      if (run === runId) parsed = preview;
    } catch {
      if (run === runId) parsed = null;
    }
  }

  /// Moves the selection and keeps the row in view.
  function move(delta: number): void {
    if (hits.length === 0) {
      selected = -1;
      return;
    }
    selected = Math.min(hits.length - 1, Math.max(0, selected + delta));
    const top = selected * ROW_H;
    const bottom = top + ROW_H;
    if (listEl) {
      if (top < listEl.scrollTop) listEl.scrollTop = top;
      else if (bottom > listEl.scrollTop + listEl.clientHeight)
        listEl.scrollTop = bottom - listEl.clientHeight;
    }
  }

  /// Enter / Ctrl+Enter: open with the default app, or reveal in the file
  /// manager, then hide. A failure keeps the window up and shows why.
  async function activate(reveal: boolean): Promise<void> {
    const hit = hits[selected];
    if (!hit) return;
    try {
      if (reveal) await revealPath(hit.id);
      else await openPath(hit.id);
      openError = null;
      void quickHide();
    } catch (err) {
      openError = String(err);
    }
  }

  /// The hit row under a mouse position: the container's own coordinate
  /// space maps straight to row indices via the fixed row height (no
  /// per-row handlers needed).
  function rowIndexAt(list: HTMLElement, event: MouseEvent): number {
    const y = event.clientY - list.getBoundingClientRect().top + list.scrollTop;
    const index = Math.floor(y / ROW_H);
    return index >= 0 && index < hits.length ? index : -1;
  }

  /// Hover selects, click opens — the launcher-style interaction.
  function onListClick(event: MouseEvent): void {
    const index = rowIndexAt(event.currentTarget as HTMLElement, event);
    if (index >= 0) {
      selected = index;
      void activate(false);
    }
  }

  function onListMousemove(event: MouseEvent): void {
    const index = rowIndexAt(event.currentTarget as HTMLElement, event);
    selected = index;
  }

  function onKeydown(event: KeyboardEvent): void {
    // IME composition (Chinese input!): Enter/arrows belong to the
    // composer until it ends — never act on them.
    if (event.isComposing) return;
    if (event.key === "Escape") {
      event.preventDefault();
      void quickHide();
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      move(1);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      move(-1);
    } else if (event.key === "Enter") {
      event.preventDefault();
      void activate(event.ctrlKey);
    }
  }

  onMount(() => {
    void initI18n();
    inputEl?.focus();
    // Empty query lists the most recent entries (the engine's own
    // empty-text behavior) — a live first impression for free.
    void runSearch().catch(() => {});

    // Re-show (window focus): keep the query text, select it, drop the
    // result selection. The hide side of focus loss is Rust's job.
    const unlisten = getCurrentWindow().onFocusChanged(({ payload: focused }) => {
      if (focused) {
        selected = -1;
        openError = null;
        inputEl?.focus();
        inputEl?.select();
      }
    });
    return () => {
      void unlisten.then((fn) => fn());
      clearTimeout(debounceTimer);
    };
  });

  function humanBytes(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    const units = ["KB", "MB", "GB", "TB"] as const;
    let value = bytes;
    let unit = -1;
    do {
      value /= 1024;
      unit++;
    } while (value >= 1024 && unit < units.length - 1);
    return `${value >= 100 ? value.toFixed(0) : value.toFixed(1)} ${units[unit]}`;
  }

  const timeFormat = new Intl.DateTimeFormat(undefined, {
    dateStyle: "short",
    timeStyle: "short",
  });
  function formatTime(unixSecs: number): string {
    return timeFormat.format(new Date(unixSecs * 1000));
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="quick">
  <div class="searchbox">
    <input
      bind:this={inputEl}
      bind:value={query}
      oninput={queueSearch}
      placeholder={$t("quick.placeholder")}
      aria-label={$t("a11y.quickInput")}
      spellcheck="false"
      autocomplete="off"
    />
  </div>

  {#if chips.length > 0}
    <div class="chips" aria-label={$t("a11y.modifierChips")}>
      {#each chips as chip (chip)}<span class="chip">{chip}</span>{/each}
    </div>
  {/if}

  {#if openError}
    <p class="error" role="alert">{openError}</p>
  {/if}

  {#if hits.length === 0}
    <div class="empty">
      {#if warming}
        <p>{$t("quick.warming")}</p>
      {:else if query.trim() === ""}
        <p class="muted">{$t("quick.hint")}</p>
      {:else}
        <p>{$t("quick.noResults")}</p>
      {/if}
    </div>
  {:else}
    <div class="meta">
      {#if query.trim() === ""}
        <span class="muted">{$t("quick.hint")}</span>
      {:else}
        <span class="muted">{total.toLocaleString()} {$t("quick.results")}</span>
      {/if}
      <span class="muted">{$t("quick.hints")}</span>
    </div>
    <!-- Keyboard interaction lives on the window-level keydown handler
         (the input keeps focus), so the listbox itself needs no key
         handling; tabindex="-1" only marks it programmatically focusable. -->
    <!-- svelte-ignore a11y_click_events_have_key_events -->
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div
      class="list"
      role="listbox"
      tabindex="-1"
      aria-label={$t("a11y.resultsList")}
      bind:this={listEl}
      bind:clientHeight={viewportH}
      onscroll={(event) => (scrollTop = event.currentTarget.scrollTop)}
      onclick={onListClick}
      onmousemove={onListMousemove}
    >
      <div class="rows" style="height: {hits.length * ROW_H}px">
        {#each visible as { hit, index } (hit.id)}
          <div
            class="row"
            class:selected={index === selected}
            style="top: {index * ROW_H}px; height: {ROW_H}px"
            role="option"
            aria-selected={index === selected}
          >
            <span class="icon" class:dir={hit.is_dir} aria-hidden="true">
              {#if hit.is_dir}
                <svg viewBox="0 0 16 16" width="16" height="16">
                  <path
                    d="M1.75 3.5c0-.69.56-1.25 1.25-1.25h3.09c.33 0 .65.13.88.37l.79.78c.23.24.55.37.88.37h4.86c.69 0 1.25.56 1.25 1.25v7.48c0 .69-.56 1.25-1.25 1.25H3c-.69 0-1.25-.56-1.25-1.25V3.5Z"
                    fill="currentColor"
                  />
                </svg>
              {:else}
                <svg viewBox="0 0 16 16" width="16" height="16">
                  <path
                    d="M4.25 1.25h4.13l3.37 3.37v9.13c0 .41-.34.75-.75.75H4.25a.75.75 0 0 1-.75-.75V2c0-.41.34-.75.75-.75Z"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="1.2"
                    stroke-linejoin="round"
                  />
                  <path
                    d="M8.5 1.25v3.5H12"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="1.2"
                    stroke-linejoin="round"
                  />
                </svg>
              {/if}
            </span>
            <span class="names">
              <span class="name">{hit.name}</span>
              <span class="path">{hit.path}</span>
            </span>
            <span class="props">
              {#if !hit.is_dir}<span>{humanBytes(hit.size)}</span>{/if}
              <span>{formatTime(hit.mtime)}</span>
            </span>
          </div>
        {/each}
      </div>
    </div>
  {/if}
</div>

<style>
  .quick {
    height: 100%;
    display: flex;
    flex-direction: column;
    background: var(--panel);
    border: 1px solid var(--border);
    overflow: hidden;
  }

  .searchbox {
    flex-shrink: 0;
    padding: 4px 12px;
    border-bottom: 1px solid var(--border);
  }

  .searchbox input {
    width: 100%;
    font: inherit;
    font-size: 16px;
    color: var(--text);
    background: transparent;
    border: none;
    outline: none;
    padding: 10px 2px;
  }

  .chips {
    flex-shrink: 0;
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    padding: 8px 12px;
    border-bottom: 1px solid var(--border);
  }

  .chip {
    font-family: ui-monospace, Consolas, monospace;
    font-size: 11px;
    color: var(--accent);
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 2px 9px;
  }

  .error {
    flex-shrink: 0;
    margin: 0;
    padding: 8px 12px;
    color: var(--error);
    font-size: 12px;
    overflow-wrap: anywhere;
  }

  .meta {
    flex-shrink: 0;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 6px 12px;
    font-size: 12px;
  }

  .muted {
    color: var(--muted);
  }

  .empty {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--text);
  }

  .list {
    flex: 1;
    overflow-y: auto;
  }

  .rows {
    position: relative;
  }

  .row {
    position: absolute;
    left: 0;
    right: 0;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 0 12px;
    font: inherit;
    color: inherit;
    background: transparent;
    border: none;
    cursor: default;
    user-select: none;
  }

  .row.selected {
    background: color-mix(in srgb, var(--accent) 13%, transparent);
  }

  .icon {
    flex-shrink: 0;
    display: flex;
    color: var(--muted);
  }

  .icon.dir {
    color: var(--accent);
  }

  .names {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }

  .name {
    font-weight: 500;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .path {
    font-size: 11px;
    color: var(--muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .props {
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 1px;
    font-size: 11px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }
</style>
