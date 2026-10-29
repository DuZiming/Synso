<script lang="ts">
  // Quick search window (Task 15): one input, one windowed result list.
  // Search runs 30ms-debounced through the `search` command; the parsed
  // modifier chips come from `parse_preview` (the engine's own parser, so
  // the chips always match what was searched). Enter opens the selection
  // with its default app, Ctrl+Enter reveals it in Explorer, Esc (or a
  // focus loss, handled Rust-side) hides the window; re-showing keeps the
  // query text, selects it, and clears the result selection.
  //
  // Task 18 multi-select: results can be gathered into a collection —
  // Ctrl/Cmd+click toggles one row, Shift+click extends the range from
  // the last anchor, and every row carries a checkbox (the mouse-only
  // path) — then 加入集合 opens the shared CollectionPicker and `coll_add`s
  // the picked ids. Plain click keeps the launcher semantics (select +
  // open). A fresh search resets the selection (it refers to the visible
  // results); the quick-query event (the collections view's 「在快速搜索
  // 中查看」 pre-fill) arrives after a Rust-side show and swaps the query.
  import { onMount } from "svelte";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { initI18n, t } from "../i18n";
  import { formatTime, humanBytes } from "../lib/format";
  import {
    collAdd,
    engineStats,
    openPath,
    parsePreview,
    quickHide,
    revealPath,
    search,
  } from "../lib/tauri";
  import type { Collection, Hit, ParsedQuery } from "../lib/types";
  import CollectionPicker from "../collections/CollectionPicker.svelte";

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

  // ---- Task 18: multi-select → 加入集合 ----
  /// Picked rows by hit id (the ids coll_add consumes). A new search
  /// clears this: a selection refers to the results it was made on.
  let picked = $state<Record<number, boolean>>({});
  /// Row index of the last click — the Shift+click range anchor.
  let anchor = $state(-1);
  /// The shared collection picker (CollectionPicker.svelte).
  let pickerOpen = $state(false);
  /// In-flight add + feedback lines (mutually exclusive with picked: a
  /// successful add clears the selection).
  let adding = $state(false);
  let addedTo = $state<string | null>(null);
  let addTimer: ReturnType<typeof setTimeout> | undefined;
  let addError = $state<string | null>(null);

  const pickedCount = $derived(
    Object.values(picked).reduce((n, on) => n + (on ? 1 : 0), 0),
  );

  function pickedIds(): number[] {
    return Object.entries(picked)
      .filter(([, on]) => on)
      .map(([id]) => Number(id));
  }

  function clearPicked(): void {
    picked = {};
    anchor = -1;
  }

  /// Ctrl/Cmd+click: toggle one row; also the checkbox path.
  function togglePicked(index: number): void {
    const hit = hits[index];
    if (!hit) return;
    picked[hit.id] = !picked[hit.id];
    anchor = index;
  }

  /// Shift+click: extend the pick from the anchor (a fresh anchor picks
  /// from the top). Range adds, never subtracts — the standard listbox
  /// extension gesture.
  function rangePicked(index: number): void {
    if (hits.length === 0) return;
    const start = anchor >= 0 ? Math.min(anchor, index) : 0;
    const end = anchor >= 0 ? Math.max(anchor, index) : index;
    for (let i = start; i <= end; i++) {
      const hit = hits[i];
      if (hit) picked[hit.id] = true;
    }
    anchor = index;
  }

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
    clearPicked();
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

  /// Hover selects, click opens — the launcher-style interaction —
  /// with the multi-select gestures layered on: Ctrl/Cmd+click toggles
  /// the row into the 加入集合 selection, Shift+click extends the range.
  function onListClick(event: MouseEvent): void {
    const index = rowIndexAt(event.currentTarget as HTMLElement, event);
    if (index >= 0) {
      if (event.ctrlKey || event.metaKey) {
        togglePicked(index);
        return;
      }
      if (event.shiftKey) {
        rangePicked(index);
        return;
      }
      anchor = index;
      selected = index;
      void activate(false);
    }
  }

  function onListMousemove(event: MouseEvent): void {
    const index = rowIndexAt(event.currentTarget as HTMLElement, event);
    selected = index;
  }

  /// Opens the shared picker over the current selection. The picker only
  /// resolves WHICH collection; the add itself happens in `onPickColl`.
  function openPicker(): void {
    if (pickedCount === 0) return;
    openError = null;
    pickerOpen = true;
  }

  async function onPickColl(coll: Collection): Promise<void> {
    const ids = pickedIds();
    if (ids.length === 0) {
      pickerOpen = false;
      return;
    }
    adding = true;
    addError = null;
    try {
      await collAdd(coll.id, ids);
      pickerOpen = false;
      clearPicked();
      addedTo = coll.name;
      clearTimeout(addTimer);
      addTimer = setTimeout(() => (addedTo = null), 10_000);
    } catch (err) {
      // Keeping the picker up is useless behind its own overlay — close it
      // and surface the failure where the results are (ids may be stale
      // after a rescan; the user re-picks and retries).
      pickerOpen = false;
      addError = String(err);
    } finally {
      adding = false;
    }
  }

  function onKeydown(event: KeyboardEvent): void {
    // IME composition (Chinese input!): Enter/arrows belong to the
    // composer until it ends — never act on them.
    if (event.isComposing) return;
    // While the collection picker is up it owns the keyboard: Esc closes
    // IT (not the window; the picker's own Esc handler closes it too —
    // idempotent), every other key stays out of the way.
    if (pickerOpen) {
      if (event.key === "Escape") {
        event.preventDefault();
        pickerOpen = false;
      }
      return;
    }
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

  /// The collections view's 「在快速搜索中查看」: the Rust side shows the
  /// window (the dashboard-button path) and emits `quick-query`; swap the
  /// query and run the search immediately (bypassing the debounce — the
  /// text arrived complete). The stale-response runId guard inside
  /// runSearch makes a racing queued keystroke harmless.
  async function applyQuery(q: string): Promise<void> {
    query = q;
    openError = null;
    pickerOpen = false;
    clearTimeout(debounceTimer);
    await runSearch().catch(() => {});
    inputEl?.focus();
  }

  onMount(() => {
    void initI18n();
    inputEl?.focus();
    // Empty query lists the most recent entries (the engine's own
    // empty-text behavior) — a live first impression for free.
    void runSearch().catch(() => {});

    // Task 18 pre-fill hook (quick.rs's quick_show_query command).
    const unlistenQuery = getCurrentWindow().listen<{ q: string }>(
      "quick-query",
      (event) => void applyQuery(event.payload.q),
    );

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
      void unlistenQuery.then((fn) => fn());
      clearTimeout(debounceTimer);
      clearTimeout(addTimer);
    };
  });
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
  {#if addError}
    <p class="error" role="alert">{addError}</p>
  {/if}
  {#if addedTo}
    <p class="added" role="status">
      {$t("collections.added").replace("{name}", addedTo)}
    </p>
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
      {#if pickedCount > 0}
        <!-- Task 18: multi-select → the shared collection picker. -->
        <button class="primary add-btn" disabled={adding} onclick={openPicker}>
          {$t("collections.addTitle")} ({pickedCount.toLocaleString()})
        </button>
      {:else}
        <span class="muted">{$t("quick.hints")}</span>
      {/if}
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
            class:picked={!!picked[hit.id]}
            style="top: {index * ROW_H}px; height: {ROW_H}px"
            role="option"
            aria-selected={index === selected}
          >
            <!-- Multi-select checkbox: the mouse-only toggle. The row
                 itself carries the keyboard-free gestures too (Ctrl+click
                 toggle, Shift+click range); the box is decorative to the
                 a11y tree on purpose — a listbox option must not nest
                 interactive descendants, and the gestures live on the
                 option's own click. -->
            <!-- svelte-ignore a11y_click_events_have_key_events -->
            <!-- svelte-ignore a11y_no_static_element_interactions -->
            <span
              class="pickbox"
              class:on={!!picked[hit.id]}
              aria-hidden="true"
              title={$t("a11y.pickBox")}
              onclick={(event) => {
                event.stopPropagation();
                togglePicked(index);
              }}
            ></span>
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

{#if pickerOpen}
  <CollectionPicker
    onpick={(coll) => void onPickColl(coll)}
    onclose={() => (pickerOpen = false)}
  />
{/if}

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

  /* Task 18: multi-select + add feedback. */
  .added {
    flex-shrink: 0;
    margin: 0;
    padding: 4px 12px;
    color: var(--warm);
    font-size: 12px;
    font-weight: 600;
  }

  .meta .add-btn {
    padding: 4px 12px;
    font-size: 12px;
  }

  .row.picked {
    background: color-mix(in srgb, var(--accent) 18%, transparent);
    box-shadow: inset 3px 0 0 var(--accent);
  }

  .pickbox {
    flex-shrink: 0;
    width: 15px;
    height: 15px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg);
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 11px;
    line-height: 1;
    color: #fff;
    cursor: pointer;
    opacity: 0;
  }

  .row:hover .pickbox,
  .pickbox.on {
    opacity: 1;
  }

  .pickbox.on {
    background: var(--accent);
    border-color: var(--accent);
  }

  .pickbox.on::after {
    content: "✓";
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
