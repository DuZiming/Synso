<script lang="ts">
  // Flame view (Task 17): the 火焰图 tab — a directory-size usage map over
  // the engine's `size_tree`. Single-chart mode by default with a
  // breadcrumb chain back to the virtual "(all)" root; 对比模式 renders two
  // independent panes, each with its own root (drill / breadcrumb / a
  // root-picker input that accepts an index path). Every visible pane is
  // (re)fetched via `size_tree(path, depth, 40)` where `depth` is the
  // user-tunable layer count (default 10, persisted per browser); an
  // unknown root renders 路径不存在 but keeps its breadcrumb so the user
  // can climb back out.
  //
  // Per-pane state machine: root changes come from the breadcrumb or the
  // root-picker input; arcs no longer drill (v1.1 user decision) — a
  // click PINS the arc's side lists instead.
  import { onMount } from "svelte";
  import { t } from "../i18n";
  import {
    copyToClipboard,
    listVolumes,
    sizeTree,
    volumeOverlay,
  } from "../lib/tauri";
  import { listen } from "@tauri-apps/api/event";
  import type { SizeNode, VolumeInfo, VolumeOverlay } from "../lib/types";
  import FlameChartSunburst from "./FlameChartSunburst.svelte";
  import { humanBytes } from "../lib/format";

  /// One level of the hover path chain (name + size, root first).
  interface ChainLevel {
    name: string;
    size: number;
  }

  /// What the side panels show for the hovered block.
  interface HoverInfo {
    chain: ChainLevel[];
    children: ChainLevel[];
  }

  /// Per-pane view state: `root` is the requested index path ("" = the
  /// virtual root), `tree` the last successful fetch, `seq` a race guard
  /// so a slow response can never overwrite a newer request, `picker` the
  /// text input's draft, `hover` the hovered block's chain (right panel)
  /// and subdirectories (left panel), `overlay` the physical-capacity
  /// figures when this pane's root names a whole volume (null elsewhere).
  interface Pane {
    root: string;
    tree: SizeNode | null;
    loading: boolean;
    missing: boolean;
    error: string | null;
    seq: number;
    picker: string;
    hover: HoverInfo | null;
    overlay: VolumeOverlay | null;
    /// Fixed volumes with their totals, fetched when this pane sits at the
    /// (all) root — the promoted volume arcs hover with their usage line.
    volumes: VolumeInfo[];
    /// Wall-clock of the last load that COMPLETED (success or error —
    /// either way the refresh FIRED; a stuck timestamp means no trigger).
    /// Displayed beside the breadcrumb so a stale map is diagnosable at
    /// a glance: time moved + map identical = refreshed-but-unchanged
    /// data; time frozen = the fs-changed trigger never landed.
    updatedAt: number | null;
    /// The click-pinned arc (see [`togglePin`]); null = follow the hover.
    pinned: { key: string; info: HoverInfo } | null;
  }

  function newPane(root: string): Pane {
    return {
      root,
      tree: null,
      loading: false,
      missing: false,
      error: null,
      seq: 0,
      picker: root,
      hover: null,
      overlay: null,
      volumes: [],
      updatedAt: null,
      pinned: null,
    };
  }

  /// HH:MM:SS for the refresh-time chip.
  function clockOf(ts: number | null): string {
    if (ts === null) return "--:--:--";
    return new Date(ts).toLocaleTimeString([], { hour12: false });
  }

  const MAX_CHILDREN = 40;
  const TOAST_MS = 10_000;
  /// Depth range and default; persisted in localStorage (a view-local
  /// display preference — not engine state).
  const DEPTH_KEY = "synso.flame.depth";
  const DEPTH_DEFAULT = 10;

  function loadDepth(): number {
    const raw = Number(localStorage.getItem(DEPTH_KEY));
    if (!Number.isInteger(raw) || raw < 1 || raw > 30) return DEPTH_DEFAULT;
    return raw;
  }

  let depth = $state(loadDepth());

  function setDepth(next: number): void {
    const clamped = Math.max(1, Math.min(30, next));
    if (clamped === depth) return;
    depth = clamped;
    localStorage.setItem(DEPTH_KEY, String(clamped));
    for (const pane of panes) void load(pane);
  }

  let compare = $state(false);
  let panes = $state<Pane[]>([newPane("")]);
  let toast = $state<string | null>(null);
  let toastTimer: ReturnType<typeof setTimeout> | undefined;

  const visiblePanes = $derived(compare ? panes : panes.slice(0, 1));

  async function load(pane: Pane): Promise<void> {
    const seq = ++pane.seq;
    pane.loading = true;
    pane.error = null;
    // The overlay rides along on every load (including fs-changed
    // refreshes) so the used/free figures track deletions; a failure
    // degrades to no overlay, never a failed pane. At the (all) root the
    // overlay is null by design — instead the volume list feeds the
    // promoted arcs' hover usage line (spec: (all) 不加块,悬停补文案).
    const overlayFetch = volumeOverlay(pane.root).catch(() => null);
    const volumesFetch =
      pane.root === ""
        ? listVolumes().catch(() => [] as VolumeInfo[])
        : Promise.resolve([] as VolumeInfo[]);
    try {
      const tree = await sizeTree(pane.root, depth, MAX_CHILDREN, false);
      const overlay = await overlayFetch;
      const volumes = await volumesFetch;
      if (seq !== pane.seq) return; // a newer request superseded this one
      pane.overlay = overlay;
      pane.volumes = volumes;
      if (tree) {
        pane.tree = tree;
        pane.missing = false;
      } else {
        pane.tree = null;
        pane.missing = true;
      }
      pane.updatedAt = Date.now();
    } catch (err) {
      if (seq !== pane.seq) return;
      pane.tree = null;
      pane.error = String(err);
      pane.updatedAt = Date.now();
    } finally {
      if (seq === pane.seq) pane.loading = false;
    }
  }

  /// At the (all) root with a hovered/pinned promoted arc: the volume
  /// that arc names, if any (the arc's own name is the scan-root segment,
  /// e.g. `C:` — matched against mounts trailing-separator-trimmed and
  /// case-insensitively, mirroring the engine's `match_volume`).
  function hoveredVolume(pane: Pane): VolumeInfo | null {
    if (pane.root !== "") return null;
    const info = listInfo(pane);
    if (!info) return null;
    const name = info.chain[0]?.name;
    if (!name) return null;
    const key = name.trimEnd().toLowerCase();
    const key2 = key.replace(/[/\\]+$/, "");
    return (
      pane.volumes.find(
        (v) => v.path.trimEnd().toLowerCase().replace(/[/\\]+$/, "") === key2,
      ) ?? null
    );
  }

  /// Root change from any source (breadcrumb, picker, drill). The engine
  /// accepts both separators; the trimmed spelling is kept as typed.
  function jump(pane: Pane, path: string): void {
    pane.root = path.trim();
    pane.picker = pane.root;
    pane.pinned = null; // the pinned arc belonged to the old view
    void load(pane);
  }

  /// Click PINS an arc (v1.1 user request: no drill, no hover dimming —
  /// click freezes the side lists on that arc). Clicking the pinned arc
  /// again unpins; clicking any other arc re-pins. Hover still drives
  /// the lists while nothing is pinned.
  function togglePin(pane: Pane, key: string, info: HoverInfo): void {
    pane.pinned = pane.pinned?.key === key ? null : { key, info };
  }

  /// The pinned arc's full index path — the chain's names root-first,
  /// joined with the separator (for volume scan roots this IS the disk
  /// path, e.g. `C:\Users\admin`).
  function pinnedPath(pane: Pane): string | null {
    const info = pane.pinned?.info;
    if (!info || info.chain.length === 0) return null;
    return info.chain.map((l) => l.name).join("\\");
  }

  /// Copies the pinned path; feedback through the toast.
  async function copyPinnedPath(pane: Pane): Promise<void> {
    const path = pinnedPath(pane);
    if (!path) return;
    try {
      await copyToClipboard(path);
      showToast($t("flame.copied"));
    } catch (err) {
      showToast(String(err));
    }
  }

  /// What the side lists show: the pinned arc when one is pinned, the
  /// live hover otherwise.
  function listInfo(pane: Pane): HoverInfo | null {
    return pane.pinned?.info ?? pane.hover;
  }

  function refresh(): void {
    for (const pane of visiblePanes) void load(pane);
  }

  function toggleCompare(): void {
    compare = !compare;
    if (!compare) return;
    if (panes.length < 2) panes.push(newPane(""));
    // Re-toggling only fetches a pane that never loaded (its root is
    // still untouched); an explored pane keeps its state.
    const right = panes[1];
    if (!right.loading && !right.tree && !right.missing && !right.error) {
      void load(right);
    }
  }

  /// Breadcrumb chain from the virtual root: "(all)" plus every segment
  /// of the current root, cumulative paths rebuilt with the separator the
  /// data itself uses (index paths arrive MAIN_SEPARATOR-joined).
  function crumbs(root: string): { name: string; path: string }[] {
    const list = [{ name: "(all)", path: "" }];
    const sep = root.includes("\\") ? "\\" : "/";
    let acc = "";
    for (const segment of root.split(/[\\/]/).filter(Boolean)) {
      acc = acc ? acc + sep + segment : segment;
      list.push({ name: segment, path: acc });
    }
    return list;
  }

  function showToast(message: string): void {
    toast = message;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toast = null), TOAST_MS);
  }

  /// Index changes (fs-changed from the engine's watcher) refresh the
  /// visible panes IN PLACE — same drill root, fresh data — so the cake
  /// tower tracks disk edits without any manual reload. Debounced: bursts
  /// (builds, copy jobs) collapse into one refetch.
  let fsDebounce: ReturnType<typeof setTimeout> | undefined;
  const unlisteners: (() => void)[] = [];

  onMount(() => {
    void load(panes[0]);
    void listen("fs-changed", () => {
      clearTimeout(fsDebounce);
      fsDebounce = setTimeout(() => {
        for (const pane of visiblePanes) void load(pane);
      }, 600);
    }).then((un) => unlisteners.push(un));
    return () => {
      clearTimeout(toastTimer);
      clearTimeout(fsDebounce);
      for (const un of unlisteners) un();
    };
  });
</script>

<section class="flame">
  <header class="flame-head">
    <label class="compare-toggle">
      <input type="checkbox" checked={compare} onchange={toggleCompare} />
      {$t("flame.compare")}
    </label>
    <div class="depth-stepper" role="group" aria-label={$t("flame.depth")}>
      <button
        class="ghost"
        onclick={() => setDepth(depth - 1)}
        disabled={depth <= 1}
        aria-label={$t("flame.depthLess")}
      >
        −
      </button>
      <span class="depth-value" title={$t("flame.depth")}>
        {$t("flame.depth")}: {depth}
      </span>
      <button
        class="ghost"
        onclick={() => setDepth(depth + 1)}
        disabled={depth >= 30}
        aria-label={$t("flame.depthMore")}
      >
        +
      </button>
    </div>
    <button class="ghost" onclick={refresh}>{$t("flame.refresh")}</button>
  </header>

  <div class="flame-panes">
    {#each visiblePanes as pane, i (i)}
      {@const list = crumbs(pane.root)}
      <div class="pane">
        <nav class="crumbs" aria-label={$t("flame.crumbsLabel")}>
          {#each list as crumb, ci (crumb.path)}
            {#if ci > 0}
              <span class="crumb-sep" aria-hidden="true">›</span>
            {/if}
            {#if ci === list.length - 1}
              <span class="crumb current" aria-current="location">{crumb.name}</span>
            {:else}
              <button class="crumb" onclick={() => jump(pane, crumb.path)}>
                {crumb.name}
              </button>
            {/if}
          {/each}
          {#if pane.loading}
            <span class="loading-chip">{$t("flame.loading")}</span>
          {/if}
        </nav>
        <input
          class="root-picker"
          type="text"
          bind:value={pane.picker}
          placeholder={$t("flame.rootPickerPlaceholder")}
          aria-label={$t("flame.rootPickerLabel")}
          onkeydown={(event) => {
            if (event.key === "Enter") jump(pane, pane.picker);
          }}
        />
        {#if pane.overlay}
          <span class="usage-line">
            {$t("flame.used")} {humanBytes(pane.overlay.used_bytes)} /
            {$t("flame.total")} {humanBytes(pane.overlay.total_bytes)}
          </span>
        {/if}
        <span class="usage-line" title={$t("flame.updatedAtHint")}>
          {$t("flame.updatedAt")} {clockOf(pane.updatedAt)}
        </span>

        {#if pane.tree}
          <div class="pane-body">
            <aside class="side-panel" aria-label={$t("flame.pathPanel")}>
              {#if listInfo(pane) && listInfo(pane)!.chain.length > 0}
                <h3>
                  {$t("flame.pathPanel")}
                  {#if pane.pinned}
                    <span class="pin-chip" title={$t("flame.pinnedHint")}>
                      📌 {$t("flame.pinned")}
                    </span>
                  {/if}
                </h3>
                {#if pane.pinned && pinnedPath(pane)}
                  <!-- The pinned arc's full path with a one-click copy. -->
                  <div class="fullpath-row">
                    <code class="fullpath" title={pinnedPath(pane)!}>
                      {pinnedPath(pane)}
                    </code>
                    <button
                      class="ghost copy-btn"
                      aria-label={$t("flame.copyPath")}
                      title={$t("flame.copyPath")}
                      onclick={() => void copyPinnedPath(pane)}
                    >
                      <svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true">
                        <rect x="5" y="1" width="9" height="11" rx="1.5"
                          fill="none" stroke="currentColor" stroke-width="1.4" />
                        <path d="M11 13.5v1a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1h1"
                          fill="none" stroke="currentColor" stroke-width="1.4" />
                      </svg>
                    </button>
                  </div>
                {/if}
                <!-- Pinned/hovered first, then each parent up to the root. -->
                <ol>
                  {#each [...listInfo(pane)!.chain].reverse() as level, i (i)}
                    <li class:current={i === 0}>
                      <span class="lvl-name" title={level.name}>{level.name}</span>
                      <span class="lvl-size">{humanBytes(level.size)}</span>
                    </li>
                  {/each}
                </ol>
                {#if hoveredVolume(pane)}
                  <p class="usage-line">
                    {$t("flame.used")}
                    {humanBytes(
                      hoveredVolume(pane)!.total_bytes -
                        hoveredVolume(pane)!.available_bytes,
                    )}
                    /
                    {$t("flame.total")}
                    {humanBytes(hoveredVolume(pane)!.total_bytes)}
                  </p>
                {/if}
              {:else}
                <p class="path-hint">{$t("flame.pathHint")}</p>
              {/if}
            </aside>
            <FlameChartSunburst
              root={pane.tree}
              overlay={pane.overlay
                ? {
                    unindexedBytes: pane.overlay.unindexed_bytes,
                    freeBytes: pane.overlay.available_bytes,
                    freeLabel: $t("flame.freeSpace"),
                    unindexedLabel: $t("flame.unindexed"),
                    centerTitle: list.length
                      ? list[list.length - 1].name
                      : pane.root,
                    // Center copy stays bare numbers (the 已用/共 words
                    // live in the header usage line) — the widened hole
                    // is still too narrow for the full sentence.
                    centerLine: `${humanBytes(pane.overlay.used_bytes)} / ${humanBytes(
                      pane.overlay.total_bytes,
                    )}`,
                  }
                : null}
              onPin={(key, info) => togglePin(pane, key, info)}
              onHover={(info) => (pane.hover = info)}
            />
            <aside class="side-panel" aria-label={$t("flame.childrenPanel")}>
              {#if listInfo(pane) && listInfo(pane)!.children.length > 0}
                <h3>{$t("flame.childrenPanel")}</h3>
                <ol>
                  {#each listInfo(pane)!.children as child (child.name)}
                    <li>
                      <span class="lvl-name" title={child.name}>{child.name}</span>
                      <span class="lvl-size">{humanBytes(child.size)}</span>
                    </li>
                  {/each}
                </ol>
              {:else if listInfo(pane)}
                <p class="path-hint">{$t("flame.childrenEmpty")}</p>
              {:else}
                <p class="path-hint">{$t("flame.childrenHint")}</p>
              {/if}
            </aside>
          </div>
        {:else if pane.loading}
          <div class="pane-state muted">{$t("flame.loading")}</div>
        {:else if pane.missing}
          <div class="pane-state">{$t("flame.missing")}</div>
        {:else if pane.error}
          <div class="pane-state error" role="alert">
            {$t("flame.loadFailed")}: {pane.error}
          </div>
        {/if}
      </div>
    {/each}
  </div>

  {#if toast}
    <div class="toast" role="status">
      <span>{toast}</span>
      <button class="toast-x" aria-label={$t("a11y.dismiss")} onclick={() => (toast = null)}
        >×</button
      >
    </div>
  {/if}
</section>

<style>
  .flame {
    margin-top: 20px;
  }

  .flame-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    flex-wrap: wrap;
  }

  .depth-stepper {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }

  .depth-value {
    font-size: 12.5px;
    min-width: 64px;
    text-align: center;
    color: var(--text);
  }

  .compare-toggle {
    display: flex;
    align-items: center;
    gap: 8px;
    font-weight: 600;
    cursor: pointer;
  }

  .compare-toggle input {
    width: 16px;
    height: 16px;
    accent-color: var(--accent);
  }

  .flame-panes {
    display: flex;
    flex-wrap: wrap;
    gap: 14px;
    margin-top: 14px;
    align-items: stretch;
    /* User requirement: the chart uses the remaining window height —
       anchor the panes to the viewport instead of content height. */
    height: calc(100vh - 240px);
    min-height: 320px;
  }

  .pane {
    flex: 1 1 0;
    min-width: 260px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .pane-body {
    flex: 1 1 auto;
    min-height: 0;
    display: flex;
    gap: 8px;
    align-items: stretch;
  }

  .pane-body > :global(.flame-wrap) {
    flex: 1 1 auto;
    min-width: 0;
  }

  .side-panel {
    flex: 0 0 200px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--panel);
    padding: 10px 12px;
    font-size: 12px;
    overflow-y: auto;
  }

  .side-panel h3 {
    margin: 0 0 6px;
    font-size: 12px;
    font-weight: 600;
    opacity: 0.7;
  }

  .side-panel ol {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }

  .side-panel li {
    display: flex;
    align-items: baseline;
    gap: 6px;
    padding: 2px 4px;
    border-radius: 4px;
  }

  .side-panel li.current {
    background: var(--bg);
    font-weight: 600;
  }

  .lvl-name {
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .lvl-size {
    color: var(--text);
    opacity: 0.85;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }

  .path-hint {
    margin: 6px 0 0;
    opacity: 0.5;
    line-height: 1.6;
  }

  .crumbs {
    display: flex;
    align-items: center;
    gap: 4px;
    flex-wrap: wrap;
    min-height: 28px;
  }

  .crumb {
    font: inherit;
    font-size: 13px;
    color: var(--accent);
    background: none;
    border: none;
    padding: 3px 6px;
    border-radius: 6px;
    cursor: pointer;
    overflow-wrap: anywhere;
    text-align: left;
  }

  .crumb:hover {
    background: var(--bg);
  }

  .crumb.current {
    color: var(--text);
    font-weight: 600;
    cursor: default;
    padding: 3px 6px;
  }

  .crumb-sep {
    color: var(--muted);
  }

  .loading-chip {
    font-size: 12px;
    color: var(--muted);
    padding: 2px 10px;
    border: 1px solid var(--border);
    border-radius: 999px;
  }

  .root-picker {
    font: inherit;
    color: var(--text);
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 7px 12px;
    width: 100%;
  }

  .usage-line {
    font-size: 12px;
    color: var(--text-muted, var(--text));
    opacity: 0.75;
    white-space: nowrap;
  }

  .pin-chip {
    font-size: 11px;
    font-weight: 500;
    margin-left: 8px;
    padding: 1px 8px;
    border-radius: 999px;
    border: 1px solid var(--border);
    color: var(--text-muted, var(--text));
  }

  .fullpath-row {
    display: flex;
    align-items: center;
    gap: 6px;
    margin: 4px 0 8px;
  }

  .fullpath {
    flex: 1;
    min-width: 0;
    font-size: 11px;
    padding: 4px 8px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg, transparent);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    direction: rtl; /* long paths show their TAIL (the leaf) */
    text-align: left;
  }

  .copy-btn {
    flex-shrink: 0;
    display: inline-flex;
    align-items: center;
    padding: 4px 7px;
  }

  .pane-state {
    background: var(--panel);
    border: 1px dashed var(--border);
    border-radius: 12px;
    padding: 40px 16px;
    text-align: center;
  }

  .muted {
    color: var(--muted);
  }

  .error {
    color: var(--error);
    overflow-wrap: anywhere;
  }
</style>
