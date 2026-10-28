<script lang="ts">
  // Dedup view (Task 16): the 查重 tab. On mount it reads `dedup_state`
  // (so a background auto-start run is picked up mid-flight) and loads the
  // last run's groups page by page; `dedup-progress` / `dedup-finished`
  // events then drive the progress card, and `fs-changed` re-syncs groups
  // when files change underneath (restores, external deletes).
  //
  // Deletion composes the engine's `delete_to_trash` purely in the
  // frontend: per group, 保留最新 / 保留最旧 pick the mtime extreme and
  // queue the rest, 勾选删除 takes the checked members — every path lands
  // in ONE confirm dialog (exact count + bytes + recycle-bin note) before
  // the command fires. The engine never materializes a 1-member group,
  // but the view still refuses to queue every member of a group: at
  // least one file per group always survives.
  import { onMount } from "svelte";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { t } from "../i18n";
  import { formatTime, humanBytes } from "../lib/format";
  import {
    dedupGroups,
    dedupStart,
    dedupState,
    deleteToTrash,
  } from "../lib/tauri";
  import type { DedupPhase, DupeGroup } from "../lib/types";

  /** Overview stats live in the parent; fires after our deletes so the
   * tiles re-read `engine_stats` (the index was already synced). */
  let { onchanged }: { onchanged?: () => void } = $props();

  /// Groups per list page (the brief's 20).
  const PAGE_SIZE = 20;
  /// Groups fetched per IPC call while loading the whole list.
  const FETCH_CHUNK = 50;
  /// Safety cap for the fetch loop (50 × 2000 = 100k groups).
  const MAX_FETCH_PAGES = 2000;

  let loading = $state(true);
  let loadError = $state<string | null>(null);

  let phase = $state<DedupPhase>("idle");
  let done = $state(0);
  let total = $state(0);
  let errors = $state(0);
  let starting = $state(false);

  let groups = $state<DupeGroup[]>([]);
  let page = $state(0);

  /** Group indexes currently expanded (member list visible). */
  let expanded = $state<number[]>([]);
  /** Checked members for manual deletion, keyed `${groupIndex}:${id}`. */
  let checked = $state<Record<string, boolean>>({});

  /** Queued deletion awaiting confirmation: ids are member row ids, bytes
   * the exact size to be freed, `all` true when the selection covers every
   * member of its group (the dialog refuses that with a hint). */
  let pending = $state<{
    ids: number[];
    bytes: number;
    all: boolean;
  } | null>(null);
  let deleting = $state(false);
  let deleteError = $state<string | null>(null);
  /// Files actually moved by the last confirmed delete (null = nothing to
  /// show); auto-clears so a stale success never outlives its context.
  let deletedCount = $state<number | null>(null);
  let resultTimer: ReturnType<typeof setTimeout> | undefined;

  const running = $derived(
    phase === "grouping" ||
      phase === "hashing_first" ||
      phase === "hashing_full",
  );
  const wastedTotal = $derived(
    groups.reduce((sum, g) => sum + wastedOf(g), 0),
  );
  const pageCount = $derived(
    Math.max(1, Math.ceil(groups.length / PAGE_SIZE)),
  );
  const safePage = $derived(Math.min(page, pageCount - 1));
  const pageGroups = $derived(
    groups.slice(safePage * PAGE_SIZE, (safePage + 1) * PAGE_SIZE),
  );

  function wastedOf(group: DupeGroup): number {
    return group.size * (group.members.length - 1);
  }

  function toggleExpanded(gi: number): void {
    expanded = expanded.includes(gi)
      ? expanded.filter((i) => i !== gi)
      : [...expanded, gi];
  }

  function checkedCountIn(gi: number, group: DupeGroup): number {
    return group.members.filter((m) => checked[`${gi}:${m.id}`]).length;
  }

  /** Opens the ONE confirm dialog shared by every deletion path. */
  function openConfirm(ids: number[], group: DupeGroup): void {
    pending = {
      ids,
      bytes: ids.length * group.size,
      all: ids.length === group.members.length,
    };
    deleteError = null;
  }

  /** 保留最新 / 保留最旧: keep one mtime extreme, queue the rest. Ties
   * keep the first member in the group's stable order. */
  function requestKeep(gi: number, group: DupeGroup, newest: boolean): void {
    const members = group.members;
    if (members.length < 2) return;
    let keep = 0;
    for (let i = 1; i < members.length; i++) {
      if (
        newest
          ? members[i].mtime > members[keep].mtime
          : members[i].mtime < members[keep].mtime
      ) {
        keep = i;
      }
    }
    const ids = members.filter((_, i) => i !== keep).map((m) => m.id);
    if (ids.length === 0) return;
    openConfirm(ids, group);
  }

  /** Manual path: checkboxes decide; the dialog refuses an all-member
   * selection (the hint shows on its disabled confirm button). */
  function requestManual(gi: number, group: DupeGroup): void {
    const ids = group.members
      .filter((m) => checked[`${gi}:${m.id}`])
      .map((m) => m.id);
    if (ids.length === 0) return;
    openConfirm(ids, group);
  }

  async function confirmDelete(): Promise<void> {
    const request = pending;
    if (!request || deleting || request.all || request.ids.length === 0) return;
    deleting = true;
    deleteError = null;
    try {
      const moved = await deleteToTrash(request.ids);
      pending = null;
      checked = {};
      deletedCount = moved;
      clearTimeout(resultTimer);
      resultTimer = setTimeout(() => (deletedCount = null), 10_000);
      // The engine synced its index synchronously; pull the dissolved /
      // shrunk groups back in and let the overview tiles re-read stats.
      await refresh();
      onchanged?.();
    } catch (err) {
      deleteError = String(err);
    } finally {
      deleting = false;
    }
  }

  async function startAnalysis(): Promise<void> {
    if (starting || running) return;
    starting = true;
    try {
      await dedupStart();
      // `dedup_start` reaches Grouping synchronously; re-read so the
      // progress card replaces the button immediately.
      await refreshState();
    } finally {
      starting = false;
    }
  }

  async function refreshState(): Promise<void> {
    const state = await dedupState();
    phase = state.phase;
    done = state.done;
    total = state.total;
    errors = state.errors;
  }

  /** Loads every group (pages until a short page) and resets the pager.
   * The engine pages size-descending with a stable order, so client-side
   * pagination over the materialized list matches the engine's own. */
  async function loadGroups(): Promise<void> {
    const acc: DupeGroup[] = [];
    for (
      let offset = 0;
      acc.length < FETCH_CHUNK * MAX_FETCH_PAGES;
      offset += FETCH_CHUNK
    ) {
      const chunk = await dedupGroups(offset, FETCH_CHUNK);
      acc.push(...chunk);
      if (chunk.length < FETCH_CHUNK) break;
    }
    groups = acc;
    page = 0;
  }

  async function refresh(): Promise<void> {
    try {
      await Promise.all([refreshState(), loadGroups()]);
      loadError = null;
    } catch (err) {
      loadError = String(err);
    } finally {
      loading = false;
    }
  }

  function onWindowKeydown(event: KeyboardEvent): void {
    if (event.key === "Escape" && pending && !deleting) pending = null;
  }

  onMount(() => {
    void refresh();

    const unlisteners: UnlistenFn[] = [];
    const on = <P,>(name: string, handler: (payload: P) => void): void => {
      void listen<P>(name, (event) => handler(event.payload)).then((un) =>
        unlisteners.push(un),
      );
    };
    on(
      "dedup-progress",
      (payload: { phase: DedupPhase; done: number; total: number }) => {
        phase = payload.phase;
        done = payload.done;
        total = payload.total;
      },
    );
    // The state machine reached Done before this fired, so the fresh
    // groups are already servable — reload them (state included, for the
    // per-run error count).
    on("dedup-finished", () => void refresh());
    // External file changes (a recycle-bin restore, another process):
    // re-sync groups once the watcher's debounce has settled.
    let fsTimer: ReturnType<typeof setTimeout> | undefined;
    on("fs-changed", () => {
      clearTimeout(fsTimer);
      fsTimer = setTimeout(() => void refresh(), 500);
    });

    return () => {
      for (const un of unlisteners) un();
      clearTimeout(fsTimer);
      clearTimeout(resultTimer);
    };
  });
</script>

<svelte:window onkeydown={onWindowKeydown} />

<section class="dedup">
  <header class="dedup-head">
    <p class="dedup-stats" aria-live="polite">
      {#if loading}
        …
      {:else if loadError}
        <span class="error" role="alert">{loadError}</span>
      {:else}
        <span>
          {#if phase === "done"}
            <span class="muted">{$t("dedup.phase.done")} · </span>
          {/if}
          {groups.length.toLocaleString()} {$t("dedup.statsGroups")}
          · {$t("dedup.wasted")} {humanBytes(wastedTotal)}
        </span>
        {#if errors > 0}
          <span class="muted">
            {$t("dedup.skippedPrefix")} {errors.toLocaleString()}
            {$t("dedup.filesUnit")}
          </span>
        {/if}
      {/if}
    </p>
    {#if !running}
      <button
        class={phase === "idle" ? "primary" : "ghost"}
        onclick={startAnalysis}
        disabled={loading || starting}
      >
        {phase === "idle" ? $t("dedup.start") : $t("dedup.rerun")}
      </button>
    {/if}
  </header>

  {#if running}
    <div class="dedup-progress-card" role="status">
      <span class="dedup-phase">{$t(`dedup.phase.${phase}`)}</span>
      <div class="scanbar">
        {#if total > 0}
          <progress
            class="bar"
            aria-label={$t("a11y.dedupProgress")}
            value={done}
            max={total}
          ></progress>
        {:else}
          <progress
            class="bar"
            aria-label={$t("a11y.dedupProgress")}
          ></progress>
        {/if}
        <span class="count">
          {done.toLocaleString()} / {total.toLocaleString()}
        </span>
      </div>
    </div>
  {/if}

  {#if !loading && !loadError && !running && groups.length === 0}
    <!-- Idle: nothing analyzed yet (not the same as "no dupes"). -->
    <p class="dedup-empty muted">
      {phase === "done" ? $t("dedup.empty") : $t("dedup.phase.idle")}
    </p>
  {:else if !loading && !loadError && (groups.length > 0 || running)}
    {#if running && groups.length > 0}
      <p class="muted dedup-stale">{$t("dedup.stale")}</p>
    {/if}
    <ul class="groups">
      {#each pageGroups as group, i (safePage * PAGE_SIZE + i)}
        {@const gi = safePage * PAGE_SIZE + i}
        {@const numChecked = checkedCountIn(gi, group)}
        <li class="group">
          <div class="group-head">
            <button
              class="group-toggle"
              aria-expanded={expanded.includes(gi)}
              onclick={() => toggleExpanded(gi)}
            >
              <span class="chev" class:open={expanded.includes(gi)}>▸</span>
              <span class="group-size">{humanBytes(group.size)}</span>
              <span class="muted">
                {group.members.length} {$t("dedup.members")}
              </span>
              <span class="muted">
                {$t("dedup.wasted")} {humanBytes(wastedOf(group))}
              </span>
            </button>
            <div class="group-actions">
              <button
                class="ghost small"
                title={$t("dedup.keepNewestHint")}
                disabled={group.members.length < 2}
                onclick={() => requestKeep(gi, group, true)}
              >
                {$t("dedup.keepNewest")}
              </button>
              <button
                class="ghost small"
                title={$t("dedup.keepOldestHint")}
                disabled={group.members.length < 2}
                onclick={() => requestKeep(gi, group, false)}
              >
                {$t("dedup.keepOldest")}
              </button>
              <button
                class="danger small"
                disabled={numChecked === 0}
                onclick={() => requestManual(gi, group)}
              >
                {$t("dedup.manualDelete")}{numChecked > 0 ? ` (${numChecked})` : ""}
              </button>
            </div>
          </div>
          {#if expanded.includes(gi)}
            <ul class="members">
              {#each group.members as m (m.id)}
                <li>
                  <label class="member">
                    <input
                      type="checkbox"
                      bind:checked={checked[`${gi}:${m.id}`]}
                      aria-label={m.name}
                    />
                    <span class="member-name">{m.name}</span>
                    <span class="member-path muted">{m.path}</span>
                    <span class="member-meta muted">
                      {humanBytes(m.size)} · {formatTime(m.mtime)}
                    </span>
                  </label>
                </li>
              {/each}
            </ul>
          {/if}
        </li>
      {/each}
    </ul>
    {#if pageCount > 1}
      <nav class="pager" aria-label={$t("tabs.dedup")}>
        <button
          class="ghost small"
          aria-label={$t("a11y.prevPage")}
          disabled={safePage === 0}
          onclick={() => (page -= 1)}
        >
          ‹
        </button>
        <span class="count">{safePage + 1} / {pageCount}</span>
        <button
          class="ghost small"
          aria-label={$t("a11y.nextPage")}
          disabled={safePage >= pageCount - 1}
          onclick={() => (page += 1)}
        >
          ›
        </button>
      </nav>
    {/if}
  {/if}

  {#if deletedCount !== null}
    <p class="dedup-result" role="status">
      {$t("dedup.deletedPrefix")} {deletedCount.toLocaleString()}
      {$t("dedup.filesUnit")}
    </p>
  {/if}
  {#if deleteError}
    <p class="error" role="alert">
      {$t("dedup.deleteFailed")}: {deleteError}
    </p>
  {/if}
</section>

{#if pending}
  <div class="confirm-overlay">
    <div
      class="confirm"
      role="alertdialog"
      aria-modal="true"
      aria-label={$t("a11y.confirmDialog")}
    >
      <h3>{$t("dedup.confirmTitle")}</h3>
      <p class="confirm-counts">
        {pending.ids.length.toLocaleString()} {$t("dedup.filesUnit")}
        · {humanBytes(pending.bytes)}
      </p>
      <p class="muted">{$t("dedup.confirmTrash")}</p>
      {#if pending.all}
        <p class="error" role="alert">{$t("dedup.confirmAllHint")}</p>
      {/if}
      <footer class="confirm-actions">
        <button
          class="ghost"
          onclick={() => (pending = null)}
          disabled={deleting}
        >
          {$t("dedup.confirmCancel")}
        </button>
        <button
          class="danger"
          disabled={deleting || pending.all || pending.ids.length === 0}
          onclick={confirmDelete}
        >
          {$t("dedup.confirmDelete")}
        </button>
      </footer>
    </div>
  </div>
{/if}

<style>
  .dedup {
    margin-top: 20px;
  }

  .dedup-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    flex-wrap: wrap;
  }

  .dedup-stats {
    margin: 0;
    display: flex;
    gap: 14px;
    flex-wrap: wrap;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }

  .muted {
    color: var(--muted);
    font-weight: 400;
  }

  .dedup-progress-card {
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 14px 18px;
    margin: 14px 0;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .dedup-phase {
    font-weight: 600;
  }

  .dedup-stale {
    margin: 10px 0 0;
    font-size: 12px;
  }

  .dedup-empty {
    margin: 24px 0;
    text-align: center;
  }

  .groups {
    list-style: none;
    padding: 0;
    margin: 14px 0 0;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .group {
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 12px;
    overflow: hidden;
  }

  .group-head {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 10px 8px 4px;
    flex-wrap: wrap;
  }

  .group-toggle {
    font: inherit;
    display: flex;
    align-items: center;
    gap: 10px;
    flex: 1;
    min-width: 0;
    padding: 6px;
    border: none;
    background: none;
    color: var(--text);
    cursor: pointer;
    text-align: left;
  }

  .group-size {
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }

  .chev {
    display: inline-block;
    transition: transform 0.15s ease;
    color: var(--muted);
  }

  .chev.open {
    transform: rotate(90deg);
  }

  .group-actions {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }

  button.small {
    padding: 5px 12px;
    font-size: 13px;
  }

  button.danger {
    font: inherit;
    color: #fff;
    background: var(--error);
    border: none;
    border-radius: 8px;
    padding: 9px 18px;
    cursor: pointer;
  }

  button.danger:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .members {
    list-style: none;
    padding: 0;
    margin: 0 0 8px;
    border-top: 1px solid var(--border);
  }

  .member {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 16px;
    cursor: pointer;
    min-width: 0;
  }

  .member:hover {
    background: var(--bg);
  }

  .member input {
    width: 16px;
    height: 16px;
    accent-color: var(--accent);
    flex-shrink: 0;
  }

  .member-name {
    font-weight: 600;
    flex-shrink: 0;
    max-width: 40%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .member-path {
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    direction: rtl;
    text-align: left;
    flex: 1;
    min-width: 0;
  }

  .member-meta {
    flex-shrink: 0;
    font-size: 12px;
    font-variant-numeric: tabular-nums;
  }

  .pager {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 12px;
    margin-top: 14px;
  }

  .dedup-result {
    margin: 14px 0 0;
    color: var(--warm);
    font-weight: 600;
  }

  .confirm-overlay {
    position: fixed;
    inset: 0;
    background: rgb(0 0 0 / 0.4);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 20;
  }

  .confirm {
    background: var(--panel);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 20px 22px;
    width: min(420px, calc(100vw - 48px));
    box-shadow: 0 10px 40px rgb(0 0 0 / 0.25);
  }

  .confirm h3 {
    margin: 0 0 10px;
    font-size: 17px;
  }

  .confirm-counts {
    font-size: 16px;
    font-weight: 600;
    margin: 0 0 6px;
    font-variant-numeric: tabular-nums;
  }

  .confirm p {
    margin: 0 0 8px;
  }

  .confirm-actions {
    display: flex;
    justify-content: flex-end;
    gap: 10px;
    margin-top: 16px;
  }
</style>
