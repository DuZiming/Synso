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
  // The synthetic "(other)" aggregate node is not drillable (it names no
  // real directory — the engine would answer None): clicking it shows a
  // toast instead of a missing-path pane. Reveal goes through
  // `reveal_index_path`; failures surface in the same toast.
  //
  // Task 18: the context menu's 加入集合 resolves the node's path → row
  // id (`file_id_of`, directories included) and opens the shared
  // CollectionPicker; the NODE itself — dir or file, not its children —
  // becomes the member. An unresolvable path (synthetic or vanished)
  // toasts instead of opening an empty picker.
  import { onMount } from "svelte";
  import { t } from "../i18n";
  import { collAdd, fileIdOf, revealIndexPath, sizeTree } from "../lib/tauri";
  import type { Collection, SizeNode } from "../lib/types";
  import FlameChartSunburst from "./FlameChartSunburst.svelte";
  import CollectionPicker from "../collections/CollectionPicker.svelte";

  /// Per-pane view state: `root` is the requested index path ("" = the
  /// virtual root), `tree` the last successful fetch, `seq` a race guard
  /// so a slow response can never overwrite a newer request, `picker` the
  /// text input's draft.
  interface Pane {
    root: string;
    tree: SizeNode | null;
    loading: boolean;
    missing: boolean;
    error: string | null;
    seq: number;
    picker: string;
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
  /// The pending add-to-collection: the resolved member ids awaiting a
  /// collection choice (null = picker closed).
  let pickerIds = $state<number[] | null>(null);

  const visiblePanes = $derived(compare ? panes : panes.slice(0, 1));

  function newPane(root: string): Pane {
    return {
      root,
      tree: null,
      loading: false,
      missing: false,
      error: null,
      seq: 0,
      picker: root,
    };
  }

  async function load(pane: Pane): Promise<void> {
    const seq = ++pane.seq;
    pane.loading = true;
    pane.error = null;
    try {
      const tree = await sizeTree(pane.root, depth, MAX_CHILDREN, false);
      if (seq !== pane.seq) return; // a newer request superseded this one
      if (tree) {
        pane.tree = tree;
        pane.missing = false;
      } else {
        pane.tree = null;
        pane.missing = true;
      }
    } catch (err) {
      if (seq !== pane.seq) return;
      pane.tree = null;
      pane.error = String(err);
    } finally {
      if (seq === pane.seq) pane.loading = false;
    }
  }

  /// Root change from any source (breadcrumb, picker, drill). The engine
  /// accepts both separators; the trimmed spelling is kept as typed.
  function jump(pane: Pane, path: string): void {
    pane.root = path.trim();
    pane.picker = pane.root;
    void load(pane);
  }

  function findNode(tree: SizeNode, path: string): SizeNode | null {
    if (tree.path === path) return tree;
    for (const child of tree.children) {
      const found = findNode(child, path);
      if (found) return found;
    }
    return null;
  }

  function drill(pane: Pane, path: string): void {
    // The engine's own tree tells us whether this path is the synthetic
    // aggregate (a real dir could legitimately carry the name).
    if (pane.tree && findNode(pane.tree, path)?.name === "(other)") {
      showToast($t("flame.aggregateNoDrill"));
      return;
    }
    jump(pane, path);
  }

  async function reveal(path: string): Promise<void> {
    try {
      await revealIndexPath(path);
    } catch (err) {
      showToast(String(err));
    }
  }

  /// Task 18 context-menu path: resolve the node's path to its files-row
  /// id (the node ITSELF joins — dir or file), then open the picker. The
  /// resolution error toasts verbatim (synthetic or vanished paths).
  async function addToCollection(path: string): Promise<void> {
    try {
      const id = await fileIdOf(path);
      pickerIds = [id];
    } catch (err) {
      showToast(String(err));
    }
  }

  async function onPickColl(coll: Collection): Promise<void> {
    const ids = pickerIds;
    if (!ids || ids.length === 0) return;
    try {
      await collAdd(coll.id, ids);
      pickerIds = null;
      showToast(
        $t("collections.added").replace("{name}", coll.name),
      );
    } catch (err) {
      showToast(String(err));
      pickerIds = null;
    }
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

  onMount(() => {
    void load(panes[0]);
    return () => clearTimeout(toastTimer);
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

        {#if pane.tree}
          <FlameChartSunburst
            root={pane.tree}
            onDrill={(path) => drill(pane, path)}
            onReveal={(path) => void reveal(path)}
            onAddToCollection={(path) => void addToCollection(path)}
          />
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

{#if pickerIds}
  <CollectionPicker
    onpick={(coll) => void onPickColl(coll)}
    onclose={() => (pickerIds = null)}
  />
{/if}

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

  .pane > :global(.flame-wrap) {
    flex: 1 1 auto;
    min-height: 0;
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
