<script lang="ts">
  // Collections view (Task 18): the 集合 tab — a virtual organization
  // layer over the index (files stay where they are; one file can belong
  // to any number of collections). Left sidebar = the collection list
  // (name + ALIVE member-count badge); right pane = the selected
  // collection's members, with tombstoned ones grayed under the
  // 「已丢失，文件被移走或删除」 badge until a rescan rebinds them.
  //
  // Per the controller ruling the sidebar badges come from ONE
  // coll_members fetch per collection per list refresh — never per
  // keystroke (there is no keystroke driving refreshes at all: they run
  // on mount, after mutations, and debounced on fs-changed/scan-finished,
  // which is exactly when the engine has already reconciled tombstones).
  //
  // Adding files happens OUT of this view — the quick window's
  // multi-select 加入集合 picker and the flame chart's context menu own
  // coll_add. Here a member can be revealed in Explorer (alive only) or
  // removed from the collection (alive or dead). Deleting a collection
  // never touches files; the confirm dialog says so in so many words.
  //
  // 「在快速搜索中查看」 opens the quick window pre-filtered with
  // `coll:<name>` via quick_show_query (the same show path as the
  // dashboard button, plus a query pre-fill event — see quick.rs).
  import { onMount } from "svelte";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { t } from "../i18n";
  import { formatTime, humanBytes } from "../lib/format";
  import {
    collCreate,
    collDelete,
    collList,
    collMembers,
    collRemove,
    collRename,
    quickShowQuery,
    revealPath,
  } from "../lib/tauri";
  import type { Collection, MemberHit } from "../lib/types";

  /// External-change refreshes wait out the watcher's batch debounce.
  const FS_DEBOUNCE_MS = 500;

  let colls = $state<Collection[]>([]);
  /// ALIVE member count per collection id, refreshed WITH the list (one
  /// members fetch per collection per refresh — see module comment).
  let counts = $state<Record<number, number>>({});
  let loading = $state(true);
  let listError = $state<string | null>(null);

  let selectedId = $state<number | null>(null);
  let members = $state<MemberHit[] | null>(null);
  let membersLoading = $state(false);
  let memberError = $state<string | null>(null);

  // Inline create (sidebar input + Enter).
  let newName = $state("");
  let creating = $state(false);
  let createError = $state<string | null>(null);

  // Inline rename (double-click the name, or the ✎ button).
  let editingId = $state<number | null>(null);
  let editName = $state("");
  let editError = $state<string | null>(null);
  let editEl: HTMLInputElement | undefined = $state();

  // Delete confirm dialog.
  let pendingDelete = $state<Collection | null>(null);
  let deleting = $state(false);
  let deleteError = $state<string | null>(null);

  // Member-row operation feedback.
  let memberBusy = $state<number | null>(null);
  let revealError = $state<string | null>(null);

  const selected = $derived(colls.find((c) => c.id === selectedId) ?? null);
  const deadCount = $derived(members?.filter((m) => !m.alive).length ?? 0);

  /// The list refresh: one coll_list + one coll_members per collection
  /// (the badge pass). Member-fetch failures degrade that badge to 0 but
  /// never take the whole list down.
  async function refreshList(): Promise<void> {
    try {
      const list = await collList();
      const entries = await Promise.all(
        list.map(async (coll) => {
          try {
            const ms = await collMembers(coll.id);
            return [coll.id, ms.reduce((n, m) => n + (m.alive ? 1 : 0), 0)] as const;
          } catch {
            return [coll.id, 0] as const;
          }
        }),
      );
      counts = Object.fromEntries(entries);
      colls = list;
      // A collection deleted elsewhere (only possible from here in v1)
      // drops its stale selection.
      if (selectedId !== null && !list.some((c) => c.id === selectedId)) {
        selectedId = null;
        members = null;
      }
      listError = null;
    } catch (err) {
      listError = String(err);
    } finally {
      loading = false;
    }
  }

  async function loadMembers(id: number): Promise<void> {
    membersLoading = true;
    memberError = null;
    revealError = null;
    try {
      members = await collMembers(id);
    } catch (err) {
      memberError = String(err);
      members = null;
    } finally {
      membersLoading = false;
    }
  }

  function select(id: number): void {
    if (selectedId === id) return;
    selectedId = id;
    editingId = null;
    members = null;
    void loadMembers(id);
  }

  async function create(): Promise<void> {
    const name = newName.trim();
    if (!name || creating) return;
    creating = true;
    createError = null;
    try {
      const created = await collCreate(name);
      newName = "";
      await refreshList();
      select(created.id);
    } catch (err) {
      createError = String(err);
    } finally {
      creating = false;
    }
  }

  function startRename(coll: Collection): void {
    editingId = coll.id;
    editName = coll.name;
    editError = null;
  }

  function cancelRename(): void {
    editingId = null;
    editError = null;
  }

  async function saveRename(): Promise<void> {
    const id = editingId;
    const name = editName.trim();
    if (id === null) return;
    const current = colls.find((c) => c.id === id);
    if (!name || !current || name === current.name) {
      cancelRename();
      return;
    }
    try {
      await collRename(id, name);
      editingId = null;
      await refreshList();
    } catch (err) {
      editError = String(err);
    }
  }

  function requestDelete(coll: Collection): void {
    pendingDelete = coll;
    deleteError = null;
  }

  async function confirmDelete(): Promise<void> {
    const coll = pendingDelete;
    if (!coll || deleting) return;
    deleting = true;
    deleteError = null;
    try {
      await collDelete(coll.id);
      pendingDelete = null;
      await refreshList();
    } catch (err) {
      deleteError = String(err);
    } finally {
      deleting = false;
    }
  }

  async function removeMember(member: MemberHit): Promise<void> {
    if (selectedId === null || memberBusy !== null) return;
    memberBusy = member.hit.id;
    memberError = null;
    try {
      await collRemove(selectedId, member.hit.id);
      await loadMembers(selectedId);
      await refreshList();
    } catch (err) {
      memberError = String(err);
    } finally {
      memberBusy = null;
    }
  }

  async function reveal(member: MemberHit): Promise<void> {
    if (!member.alive) return;
    revealError = null;
    try {
      await revealPath(member.hit.id);
    } catch (err) {
      revealError = String(err);
    }
  }

  /// Opens the quick window pre-filtered to this collection (the engine's
  /// `coll:` filter resolves ALIVE members only — tombstones can't match).
  function viewInSearch(coll: Collection): void {
    void quickShowQuery(`coll:${coll.name}`);
  }

  function onWindowKeydown(event: KeyboardEvent): void {
    if (event.key === "Escape" && pendingDelete && !deleting) pendingDelete = null;
  }

  onMount(() => {
    void refreshList();

    const unlisteners: UnlistenFn[] = [];
    const on = <P,>(name: string, handler: (payload: P) => void): void => {
      void listen<P>(name, (event) => handler(event.payload)).then((un) =>
        unlisteners.push(un),
      );
    };
    // The watcher already reconciled (tombstone/rebind) before FsChanged
    // fires — this refresh is what turns that into visible gray-out /
    // re-alive. Debounced like the dedup view.
    let fsTimer: ReturnType<typeof setTimeout> | undefined;
    on("fs-changed", () => {
      clearTimeout(fsTimer);
      fsTimer = setTimeout(() => {
        void refreshList();
        if (selectedId !== null) void loadMembers(selectedId);
      }, FS_DEBOUNCE_MS);
    });
    on("scan-finished", () => {
      void refreshList();
      if (selectedId !== null) void loadMembers(selectedId);
    });

    return () => {
      for (const un of unlisteners) un();
      clearTimeout(fsTimer);
    };
  });
</script>

<svelte:window onkeydown={onWindowKeydown} />

<section class="colls">
  <aside class="sidebar">
    <form
      class="new-row"
      onsubmit={(event) => {
        event.preventDefault();
        void create();
      }}
    >
      <input
        bind:value={newName}
        placeholder={$t("collections.createPlaceholder")}
        aria-label={$t("collections.createPlaceholder")}
        spellcheck="false"
        autocomplete="off"
        oninput={() => (createError = null)}
      />
      <button class="ghost" type="submit" disabled={!newName.trim() || creating}>
        {$t("collections.create")}
      </button>
    </form>
    {#if createError}
      <p class="error" role="alert">{createError}</p>
    {/if}

    {#if loading}
      <p class="muted side-note">…</p>
    {:else if listError}
      <p class="error" role="alert">{listError}</p>
    {:else if colls.length === 0}
      <p class="muted side-note">{$t("collections.empty")}</p>
    {:else}
      <ul class="coll-list" aria-label={$t("a11y.collList")}>
        {#each colls as coll (coll.id)}
          <li>
            {#if editingId === coll.id}
              <!-- Inline rename: Enter commits, Esc/blur cancels. -->
              <div class="rename-row">
                <input
                  bind:this={editEl}
                  bind:value={editName}
                  placeholder={$t("collections.renamePlaceholder")}
                  aria-label={$t("collections.renamePlaceholder")}
                  spellcheck="false"
                  autocomplete="off"
                  oninput={() => (editError = null)}
                  onkeydown={(event) => {
                    if (event.key === "Enter") {
                      event.preventDefault();
                      void saveRename();
                    } else if (event.key === "Escape") {
                      event.preventDefault();
                      cancelRename();
                    }
                  }}
                  onblur={cancelRename}
                />
              </div>
              {#if editError}
                <p class="error" role="alert">{editError}</p>
              {/if}
            {:else}
              <div class="coll-row" class:active={coll.id === selectedId}>
                <!-- One clickable surface selects; dblclick starts the
                     inline rename (same entry point as the ✎ button). -->
                <!-- svelte-ignore a11y_no_static_element_interactions -->
                <button
                  class="coll-name"
                  onclick={() => select(coll.id)}
                  ondblclick={() => {
                    select(coll.id);
                    startRename(coll);
                  }}
                  title={coll.name}
                >
                  <span class="coll-title">{coll.name}</span>
                  <span class="coll-count">{counts[coll.id] ?? 0}</span>
                </button>
                <span class="coll-actions">
                  <button
                    class="icon-btn"
                    aria-label={$t("collections.rename")}
                    title={$t("collections.rename")}
                    onclick={() => {
                      select(coll.id);
                      startRename(coll);
                    }}
                  >✎</button>
                  <button
                    class="icon-btn"
                    aria-label={$t("collections.delete")}
                    title={$t("collections.delete")}
                    onclick={() => requestDelete(coll)}
                  >✕</button>
                </span>
              </div>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </aside>

  <div class="detail">
    {#if selected === null}
      <div class="detail-empty muted">
        {loading ? "…" : $t("collections.empty")}
      </div>
    {:else}
      <header class="detail-head">
        <h2 class="detail-name" title={selected.name}>{selected.name}</h2>
        {#if members !== null}
          <span class="detail-count">
            {members.length.toLocaleString()} {$t("collections.membersUnit")}</span
          >
          {#if deadCount > 0}
            <span class="detail-dead muted">
              {deadCount.toLocaleString()} {$t("collections.lost")}</span
            >
          {/if}
        {/if}
        <button class="ghost view-search" onclick={() => viewInSearch(selected)}>
          {$t("collections.viewInSearch")}
        </button>
      </header>

      {#if membersLoading && members === null}
        <div class="detail-empty muted">…</div>
      {:else if memberError}
        <p class="error" role="alert">{memberError}</p>
      {:else if members !== null && members.length === 0}
        <div class="detail-empty muted">{$t("collections.noMembers")}</div>
      {:else if members !== null}
        <ul class="member-list" aria-label={$t("a11y.memberList")}>
          {#each members as member (member.hit.id)}
            <li class="member" class:dead={!member.alive}>
              <span class="icon" class:dir={member.alive && member.hit.is_dir} aria-hidden="true">
                {#if member.alive && member.hit.is_dir}
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
              <span class="member-names">
                <span class="member-name">{member.hit.name}</span>
                {#if member.alive}
                  <span class="member-path">{member.hit.path}</span>
                {:else}
                  <span class="lost-badge">{$t("collections.lost")}</span>
                {/if}
              </span>
              <span class="member-meta">
                {#if member.alive}
                  <span>{member.hit.is_dir ? "" : humanBytes(member.hit.size)}</span>
                  <span>{formatTime(member.hit.mtime)}</span>
                {:else}
                  <span>—</span>
                  <span>—</span>
                {/if}
              </span>
              <span class="member-actions">
                {#if member.alive}
                  <button
                    class="ghost small"
                    onclick={() => void reveal(member)}
                  >{$t("collections.reveal")}</button>
                {/if}
                <button
                  class="ghost small"
                  disabled={memberBusy === member.hit.id}
                  onclick={() => void removeMember(member)}
                >{$t("collections.removeMember")}</button>
              </span>
            </li>
          {/each}
        </ul>
        {#if revealError}
          <p class="error" role="alert">{revealError}</p>
        {/if}
      {/if}
    {/if}
  </div>
</section>

{#if pendingDelete}
  <div class="confirm-overlay">
    <div
      class="confirm"
      role="alertdialog"
      aria-modal="true"
      aria-label={$t("a11y.confirmDialog")}
    >
      <h3>{$t("collections.deleteTitle")}</h3>
      <p class="confirm-name">{pendingDelete.name}</p>
      <!-- Spec-critical: deleting a collection NEVER deletes files. -->
      <p class="muted">{$t("collections.deleteKeepsFiles")}</p>
      {#if deleteError}
        <p class="error" role="alert">{deleteError}</p>
      {/if}
      <footer class="confirm-actions">
        <button class="ghost" onclick={() => (pendingDelete = null)} disabled={deleting}>
          {$t("collections.confirmCancel")}
        </button>
        <button class="danger" disabled={deleting} onclick={confirmDelete}>
          {$t("collections.confirmDelete")}
        </button>
      </footer>
    </div>
  </div>
{/if}

<style>
  .colls {
    margin-top: 20px;
    display: flex;
    gap: 16px;
    align-items: flex-start;
    min-height: 320px;
  }

  /* ---------------- sidebar ---------------- */

  .sidebar {
    flex: 0 0 230px;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .new-row {
    display: flex;
    gap: 6px;
  }

  .new-row input {
    font: inherit;
    flex: 1;
    min-width: 0;
    color: var(--text);
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 7px 10px;
  }

  .side-note {
    margin: 4px 0;
    font-size: 13px;
  }

  .muted {
    color: var(--muted);
  }

  .coll-list {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .coll-row {
    display: flex;
    align-items: center;
    gap: 2px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 3px 6px 3px 3px;
  }

  .coll-row.active {
    border-color: var(--accent);
  }

  .coll-name {
    font: inherit;
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    background: none;
    border: none;
    color: var(--text);
    padding: 7px 8px;
    cursor: pointer;
    text-align: left;
  }

  .coll-title {
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .coll-count {
    flex-shrink: 0;
    font-size: 12px;
    font-variant-numeric: tabular-nums;
    color: var(--accent);
    background: color-mix(in srgb, var(--accent) 12%, transparent);
    border-radius: 999px;
    padding: 1px 8px;
  }

  .coll-actions {
    display: flex;
    flex-shrink: 0;
  }

  .icon-btn {
    font: inherit;
    font-size: 12px;
    line-height: 1;
    color: var(--muted);
    background: none;
    border: none;
    border-radius: 6px;
    padding: 5px 5px;
    cursor: pointer;
  }

  .icon-btn:hover {
    color: var(--text);
    background: var(--bg);
  }

  .rename-row input {
    font: inherit;
    width: 100%;
    color: var(--text);
    background: var(--panel);
    border: 1px solid var(--accent);
    border-radius: 10px;
    padding: 8px 10px;
  }

  /* ---------------- detail pane ---------------- */

  .detail {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .detail-empty {
    background: var(--panel);
    border: 1px dashed var(--border);
    border-radius: 12px;
    padding: 40px 16px;
    text-align: center;
  }

  .detail-head {
    display: flex;
    align-items: baseline;
    gap: 10px;
    flex-wrap: wrap;
  }

  .detail-name {
    font-size: 18px;
    margin: 0;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .detail-count {
    font-variant-numeric: tabular-nums;
    font-weight: 600;
  }

  .detail-dead {
    font-size: 12px;
  }

  .view-search {
    margin-left: auto;
    padding: 6px 12px;
    font-size: 13px;
  }

  .member-list {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .member {
    display: flex;
    align-items: center;
    gap: 10px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 8px 12px;
    min-width: 0;
  }

  /* Tombstoned member: grayed, its snapshot keeps the name visible. */
  .member.dead {
    opacity: 0.55;
    border-style: dashed;
  }

  .icon {
    flex-shrink: 0;
    display: flex;
    color: var(--muted);
  }

  .icon.dir {
    color: var(--accent);
  }

  .member-names {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }

  .member-name {
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .member-path {
    font-size: 11px;
    color: var(--muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    direction: rtl;
    text-align: left;
  }

  .lost-badge {
    align-self: flex-start;
    font-size: 11px;
    color: var(--cold);
    border: 1px solid var(--cold);
    border-radius: 999px;
    padding: 0 8px;
    margin-top: 2px;
  }

  .member-meta {
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 1px;
    font-size: 11px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }

  .member-actions {
    flex-shrink: 0;
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
    justify-content: flex-end;
  }

  button.small {
    padding: 5px 10px;
    font-size: 12px;
  }

  /* ---------------- delete confirm ---------------- */

  .confirm-overlay {
    position: fixed;
    inset: 0;
    background: rgb(0 0 0 / 0.4);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 30;
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

  .confirm-name {
    font-size: 16px;
    font-weight: 600;
    margin: 0 0 8px;
    overflow-wrap: anywhere;
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
</style>
