<script lang="ts">
  // One flame chart — custom SVG renderer (replaces d3-flame-graph, whose
  // full-width partition layout fought every requirement here):
  //
  // 1. DIRS ONLY: files are never materialized by the engine's dir-only
  //    `size_tree`, so the gap between a dir's block and its children's
  //    combined width IS that dir's direct-file share — the "mountain"
  //    taper the product wants.
  // 2. CENTERED PACKING: each level's blocks are laid adjacent, centered
  //    over their parent's center (not edge-to-edge tiling), so towers
  //    narrow toward their tips like a candle flame.
  // 3. FULL HEIGHT + ZOOM: rows divide the whole pane height; the wheel
  //    zooms horizontally around the cursor, drag pans, double-click
  //    resets.
  //
  // Pure presentation — engine I/O and state live in the parent
  // (FlameView), which re-roots by passing a new `root` (drilling) and
  // handles reveal/toasts via the callbacks.
  import { onDestroy } from "svelte";
  import { t } from "../i18n";
  import { humanBytes } from "../lib/format";
  import type { SizeNode } from "../lib/types";

  let {
    root,
    onDrill,
    onReveal,
    onAddToCollection,
  }: {
    root: SizeNode;
    onDrill: (path: string) => void;
    onReveal: (path: string) => void;
    onAddToCollection: (path: string) => void;
  } = $props();

  /// A laid-out block. Coordinates are in PANE pixels, already zoom/pan
  /// applied — the layout IS the transform, recomputed on zoom/pan (cheap:
  /// sub-pixel blocks prune their subtrees).
  interface Block {
    node: SizeNode;
    depth: number;
    x: number;
    w: number;
    pctOfParent: number;
    color: string;
  }

  const MIN_BLOCK_PX = 1;
  const MIN_LABEL_PX = 42;
  const ZOOM_MIN = 1;
  const ZOOM_MAX = 400;
  const MENU_WIDTH = 190;
  const MENU_HEIGHT = 84;
  const PAD = 8;

  // Per-depth warm/cool palette (validated for light and dark surfaces;
  // identity is never color-alone — every wide block carries its label).
  const PALETTE = {
    light: {
      warmLight: "#dfa262",
      warmDeep: "#bd7a30",
      coolLight: "#4f9fd1",
      coolDeep: "#2472a8",
    },
    dark: {
      warmLight: "#a06a25",
      warmDeep: "#96601a",
      coolLight: "#24699c",
      coolDeep: "#1667a4",
    },
  } as const;

  let wrap: HTMLDivElement | undefined = $state();
  let plotW = $state(800);
  let plotH = $state(400);
  let zoom = $state(1);
  let panX = $state(0);
  let isDark = $state(
    window.matchMedia("(prefers-color-scheme: dark)").matches,
  );
  let hover = $state<{
    node: SizeNode;
    pctOfParent: number;
    x: number;
    y: number;
  } | null>(null);
  let menu = $state<{
    x: number;
    y: number;
    path: string;
    name: string;
    synthetic: boolean;
  } | null>(null);

  let resizeObserver: ResizeObserver | null = null;

  function interpolate(a: string, b: string, fraction: number): string {
    const mix = (at: number): number =>
      Math.round(
        parseInt(a.slice(at, at + 2), 16) * (1 - fraction) +
          parseInt(b.slice(at, at + 2), 16) * fraction,
      );
    return `rgb(${mix(1)}, ${mix(3)}, ${mix(5)})`;
  }

  function blockColor(depth: number, share: number): string {
    const palette = isDark ? PALETTE.dark : PALETTE.light;
    if (depth % 2 === 0) {
      return interpolate(palette.warmDeep, palette.warmLight, share);
    }
    return interpolate(palette.coolDeep, palette.coolLight, share);
  }

  /// The whole chart: a flat block list + how many rows it spans.
  const layout = $derived.by(() => {
    const width = Math.max(1, plotW - PAD * 2);
    const height = Math.max(1, plotH - PAD * 2);
    const total = Math.max(1, root.size);
    const scale = (width * zoom) / total;
    const blocks: Block[] = [];
    let maxDepth = 0;

    // Children adjacent, centered over the parent's center; widths are
    // GLOBAL-proportioned (size/total), so towers narrow exactly by the
    // file share trapped at each level.
    const walk = (
      node: SizeNode,
      depth: number,
      center: number,
      parentSize: number,
    ): void => {
      maxDepth = Math.max(maxDepth, depth);
      const w = Math.max(0, node.size * scale);
      if (w < MIN_BLOCK_PX) return; // subtree pruned: descendants ≤ w
      const share =
        depth === 0 || parentSize === 0
          ? 1
          : Math.min(1, node.size / parentSize);
      blocks.push({
        node,
        depth,
        x: center - w / 2,
        w,
        pctOfParent: share * 100,
        color: blockColor(depth, share),
      });
      if (node.children.length === 0) return;
      const childSum = node.children.reduce((s, c) => s + c.size, 0);
      const rowW = Math.max(0, childSum * scale);
      let cursor = center - rowW / 2;
      for (const child of node.children) {
        const cw = Math.max(0, child.size * scale);
        walk(child, depth + 1, cursor + cw / 2, node.size);
        cursor += cw;
      }
    };
    walk(root, 0, width / 2 + panX, root.size);

    const rows = maxDepth + 1;
    const rowH = height / rows;
    return { blocks, rows, rowH, width, height };
  });

  function blockY(depth: number): number {
    // Row 0 (the root) sits at the BOTTOM; children stack upward.
    return PAD + layout.height - (depth + 1) * layout.rowH;
  }

  // ---------------- Zoom / pan ----------------

  let dragging = false;
  let dragMoved = 0;
  let dragLastX = 0;

  function clampPan(nextPan: number): number {
    const width = Math.max(1, plotW - PAD * 2);
    const w = width * zoom;
    return Math.min(0, Math.max(width - w, nextPan));
  }

  function onWheel(event: WheelEvent): void {
    event.preventDefault();
    const width = Math.max(1, plotW - PAD * 2);
    const cursor = event.offsetX - PAD;
    const factor = Math.exp(-event.deltaY * 0.0016);
    const next = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, zoom * factor));
    if (next === zoom) return;
    // Keep the data point under the cursor stationary.
    panX = clampPan(cursor - ((cursor - panX) * next) / zoom);
    zoom = next;
  }

  function onPointerDown(event: PointerEvent): void {
    if (event.button !== 0) return;
    dragging = true;
    dragMoved = 0;
    dragLastX = event.clientX;
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
  }

  function onPointerMove(event: PointerEvent): void {
    if (!dragging) return;
    const dx = event.clientX - dragLastX;
    dragLastX = event.clientX;
    dragMoved += Math.abs(dx);
    if (zoom > 1) panX = clampPan(panX + dx);
  }

  function onPointerUp(): void {
    dragging = false;
  }

  function resetView(): void {
    zoom = 1;
    panX = 0;
  }

  // ---------------- Interactions ----------------

  function onBlockClick(block: Block): void {
    if (dragMoved > 4) return; // that was a pan, not a click
    onDrill(block.node.path);
  }

  function onBlockHover(block: Block, event: MouseEvent): void {
    const bounds = wrap!.getBoundingClientRect();
    hover = {
      node: block.node,
      pctOfParent: block.pctOfParent,
      x: Math.min(event.clientX - bounds.left + 12, bounds.width - 240),
      y: event.clientY - bounds.top + 14,
    };
  }

  function onBlockMenu(block: Block, event: MouseEvent): void {
    event.preventDefault();
    event.stopPropagation();
    const bounds = wrap!.getBoundingClientRect();
    menu = {
      x: Math.max(
        0,
        Math.min(event.clientX - bounds.left, bounds.width - MENU_WIDTH),
      ),
      y: Math.max(
        0,
        Math.min(event.clientY - bounds.top, bounds.height - MENU_HEIGHT),
      ),
      path: block.node.path,
      name: block.node.name,
      synthetic: block.node.name === "(all)" || block.node.name === "(other)",
    };
  }

  function closeMenu(): void {
    menu = null;
  }

  // ---------------- Lifecycle ----------------

  const darkQuery = window.matchMedia("(prefers-color-scheme: dark)");

  function onThemeChange(): void {
    isDark = darkQuery.matches;
  }

  $effect(() => {
    if (!wrap) return;
    resizeObserver = new ResizeObserver(() => {
      if (!wrap) return;
      plotW = wrap.clientWidth || 800;
      plotH = wrap.clientHeight || 400;
    });
    resizeObserver.observe(wrap);
    plotW = wrap.clientWidth || 800;
    plotH = wrap.clientHeight || 400;
  });

  darkQuery.addEventListener("change", onThemeChange);
  onDestroy(() => {
    darkQuery.removeEventListener("change", onThemeChange);
    resizeObserver?.disconnect();
  });
</script>

<svelte:window
  onclick={closeMenu}
  onkeydown={(event) => {
    if (event.key === "Escape") closeMenu();
  }}
/>

<div class="flame-wrap" bind:this={wrap}>
  {#if layout.blocks.length > 0}
    <svg
      width="100%"
      height="100%"
      role="img"
      aria-label={$t("flame.chartLabel")}
      onwheel={onWheel}
      onpointerdown={onPointerDown}
      onpointermove={onPointerMove}
      onpointerup={onPointerUp}
      onpointercancel={onPointerUp}
      ondblclick={resetView}
    >
      {#each layout.blocks as block (block.node.path + block.depth)}
        <rect
          x={block.x + PAD}
          y={blockY(block.depth)}
          width={Math.max(0, block.w - 0.6)}
          height={Math.max(0, layout.rowH - 1.5)}
          rx="2"
          fill={block.color}
          class="block"
          role="button"
          tabindex="-1"
          aria-label={block.node.name}
          onclick={() => onBlockClick(block)}
          onkeydown={(e) => {
            if (e.key === "Enter" || e.key === " ") onDrill(block.node.path);
          }}
          onmousemove={(e) => onBlockHover(block, e)}
          onmouseleave={() => (hover = null)}
          oncontextmenu={(e) => onBlockMenu(block, e)}
        ></rect>
        {#if block.w >= MIN_LABEL_PX && layout.rowH >= 12}
          <text
            x={block.x + PAD + 4}
            y={blockY(block.depth) + layout.rowH / 2 + 3.5}
            class="block-label"
          >
            {block.w > block.node.name.length * 7 + 10
              ? block.node.name
              : block.node.name.slice(0, Math.max(1, Math.floor(block.w / 7)))}
          </text>
        {/if}
      {/each}
    </svg>
  {:else}
    <div class="empty">{$t("flame.noDirs")}</div>
  {/if}

  {#if hover}
    <div class="tip" style="left:{hover.x}px; top:{hover.y}px">
      <b>{hover.node.name}</b><br />
      {humanBytes(hover.node.size)}<br />
      {$t("flame.filesLine").replace("{n}", hover.node.file_count.toLocaleString())}
      · {$t("flame.ofParentLine").replace("{pct}", hover.pctOfParent.toFixed(1))}
    </div>
  {/if}

  {#if menu}
    <div
      class="ctx-menu"
      style="left:{menu.x}px; top:{menu.y}px"
      role="menu"
      aria-label={$t("flame.menuLabel")}
    >
      <button
        class="ctx-item"
        role="menuitem"
        disabled={menu.synthetic}
        title={menu.synthetic ? $t("flame.revealUnavailable") : undefined}
        onclick={() => {
          if (!menu || menu.synthetic) return;
          onReveal(menu.path);
          closeMenu();
        }}
      >
        {$t("flame.reveal")}
      </button>
      <button
        class="ctx-item"
        role="menuitem"
        onclick={() => {
          if (!menu || menu.synthetic) return;
          onAddToCollection(menu.path);
          closeMenu();
        }}
      >
        {$t("flame.addToCollection")}
      </button>
    </div>
  {/if}

  {#if zoom > 1}
    <button class="ghost reset-zoom" onclick={resetView}>
      {$t("flame.resetZoom")}（{zoom.toFixed(1)}×）
    </button>
  {/if}
</div>

<style>
  .flame-wrap {
    position: relative;
    width: 100%;
    height: 100%;
    min-height: 160px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 12px;
    overflow: hidden;
    cursor: grab;
    touch-action: none;
  }

  .flame-wrap:active {
    cursor: grabbing;
  }

  svg {
    display: block;
  }

  .block {
    stroke: rgb(0 0 0 / 0.25);
    stroke-width: 0.6;
    cursor: pointer;
  }

  @media (prefers-color-scheme: dark) {
    .block {
      stroke: rgb(255 255 255 / 0.22);
    }
  }

  .block-label {
    font-family: inherit;
    font-size: 11px;
    fill: var(--text);
    paint-order: stroke;
    stroke: var(--panel);
    stroke-width: 2.5px;
    stroke-linejoin: round;
    pointer-events: none;
    user-select: none;
  }

  .empty {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    color: var(--text-dim, var(--text));
    opacity: 0.7;
  }

  .tip {
    position: absolute;
    z-index: 20;
    max-width: 230px;
    padding: 8px 10px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: 0 6px 24px rgb(0 0 0 / 0.18);
    font-size: 12px;
    line-height: 1.45;
    pointer-events: none;
  }

  .ctx-menu {
    position: absolute;
    z-index: 30;
    min-width: 180px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: 0 6px 24px rgb(0 0 0 / 0.18);
    padding: 4px;
    display: flex;
    flex-direction: column;
  }

  .ctx-item {
    font: inherit;
    text-align: left;
    color: var(--text);
    background: none;
    border: none;
    border-radius: 6px;
    padding: 7px 12px;
    cursor: pointer;
    white-space: nowrap;
  }

  .ctx-item:hover:not(:disabled) {
    background: var(--bg);
  }

  .ctx-item:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .reset-zoom {
    position: absolute;
    right: 10px;
    top: 10px;
    z-index: 10;
    font-size: 12px;
    padding: 4px 10px;
    border-radius: 6px;
    border: 1px solid var(--border);
    background: var(--panel);
    color: var(--text);
    cursor: pointer;
  }
</style>
