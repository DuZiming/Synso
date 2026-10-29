<script lang="ts">
  // One flame chart (Task 17): renders a SizeNode subtree with
  // d3-flame-graph + d3 v7. Pure presentation — all engine I/O and state
  // live in the parent (FlameView), which re-roots by passing a new `root`
  // (drilling) and handles reveal/toasts via `onReveal`.
  //
  // Library contract (dist/d3-flamegraph.es.js): a click on any frame
  // zooms INTO it internally (siblings hidden, ancestors faded — clicking
  // the visible top frame zooms back out) AND fires our onClick, which we
  // forward as onDrill; the parent's refetch then replaces the whole
  // datum, so the built-in zoom stays as the instant visual response.
  // Hover shows a floating tooltip (the library's own, re-contented via
  // tip.html) with name / size / file count / % of parent. Right-click
  // opens our context menu (reveal + add-to-collection).
  //
  // Colors are a validated warm/cool alternating per-depth palette (light
  // and dark variants both pass the dataviz categorical checks; the
  // sub-3:1 contrast band is relieved by every frame's direct text label).
  // Every frame carries its name as a label, so identity is never
  // color-alone.
  import { onDestroy } from "svelte";
  import { get } from "svelte/store";
  import { select } from "d3";
  import flamegraph, {
    tooltip as flameTooltipModule,
    type FlameGraphChart,
    type FlameGraphTooltip,
    type FlameHierarchyFrame,
  } from "d3-flame-graph";
  // (The library's ES build imports its own d3-flamegraph.css, so the
  // frame/label/tooltip base styles arrive with the JS import.)
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

  /// What the chart renders: d3-flame-graph reads {name, value, children}
  /// (value = byte size; parent == sum of children, so no self-value
  /// correction). The extra fields ride along on `.data` for the tooltip
  /// and the color mapper. `pctOfParent` is precomputed — parent sizes
  /// are immutable here, unlike the library's zoom-mutated `d.value`.
  interface FlameFrame {
    name: string;
    value: number;
    children?: FlameFrame[];
    path: string;
    fileCount: number;
    pctOfParent: number;
  }

  const CELL_HEIGHT = 20;
  const TRANSITION_MS = 250;
  /// Frames narrower than this are not rendered at all (perf on wide trees).
  const MIN_FRAME_PX = 2;
  const FALLBACK_WIDTH = 800;
  /// Context-menu size estimate for clamping (two items).
  const MENU_WIDTH = 190;
  const MENU_HEIGHT = 84;

  // Per-depth palette cycle (depth%2 picks the family; frame share of the
  // parent picks the step — bigger = lighter). Validated with the dataviz
  // palette validator against this app's surfaces (#ffffff / #1d2129):
  // lightness band, chroma floor, CVD >= 8 and normal-vision >= 15 all
  // pass in both modes; the contrast WARN is relieved by direct labels.
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

  let el: HTMLDivElement;
  let wrap: HTMLDivElement;
  let chart: FlameGraphChart | null = null;
  let tooltip: FlameGraphTooltip | null = null;
  let resizeObserver: ResizeObserver | null = null;
  let resizeTimer: ReturnType<typeof setTimeout> | undefined;

  /// Live theme read for the color mapper (frames are SVG fills painted by
  /// d3, so they can't follow CSS custom properties).
  const darkQuery = window.matchMedia("(prefers-color-scheme: dark)");

  function toFlameFrame(node: SizeNode, parentSize: number): FlameFrame {
    const frame: FlameFrame = {
      name: node.name,
      value: node.size,
      path: node.path,
      fileCount: node.file_count,
      pctOfParent: parentSize > 0 ? (node.size / parentSize) * 100 : 100,
    };
    if (node.children.length > 0) {
      frame.children = node.children.map((child) =>
        toFlameFrame(child, node.size),
      );
    }
    return frame;
  }

  function interpolate(a: string, b: string, fraction: number): string {
    const mix = (at: number): number =>
      Math.round(
        parseInt(a.slice(at, at + 2), 16) * (1 - fraction) +
          parseInt(b.slice(at, at + 2), 16) * fraction,
      );
    return `rgb(${mix(1)}, ${mix(3)}, ${mix(5)})`;
  }

  /// Share of the parent BY DATA SIZE (stable across the library's zoom
  /// mutations of `d.value`); the root itself is 1.
  function frameColor(d: FlameHierarchyFrame<FlameFrame>): string {
    const palette = darkQuery.matches ? PALETTE.dark : PALETTE.light;
    const share =
      d.parent && d.parent.data.value > 0
        ? Math.min(1, d.data.value / d.parent.data.value)
        : 1;
    if (d.depth % 2 === 0) {
      return interpolate(palette.warmDeep, palette.warmLight, share);
    }
    return interpolate(palette.coolDeep, palette.coolLight, share);
  }

  function escapeHtml(text: string): string {
    return text
      .replaceAll("&", "&amp;")
      .replaceAll("<", "&lt;")
      .replaceAll(">", "&gt;")
      .replaceAll('"', "&quot;");
  }

  function tooltipHtml(d: FlameHierarchyFrame<FlameFrame>): string {
    const frame = d.data;
    const files = get(t)("flame.filesLine").replace(
      "{n}",
      frame.fileCount.toLocaleString(),
    );
    const share = get(t)("flame.ofParentLine").replace(
      "{pct}",
      frame.pctOfParent.toFixed(1),
    );
    return (
      `<b>${escapeHtml(frame.name)}</b>` +
      `<br>${escapeHtml(humanBytes(frame.value))}` +
      `<br>${files} · ${share}`
    );
  }

  // ---------------- Context menu ----------------

  let menu = $state<{
    x: number;
    y: number;
    path: string;
    name: string;
    synthetic: boolean;
  } | null>(null);

  /// Window-level: d3 binds only "click", so a right-click bubbles out of
  /// the frame's rect carrying the hierarchy node on `__data__`. Each
  /// FlameChart instance listens and claims only events inside its own
  /// wrap (compare mode renders two charts side by side).
  function onWindowContextMenu(event: MouseEvent): void {
    const target = event.target as HTMLElement & { __data__?: unknown };
    if (!wrap || !wrap.contains(target)) return;
    const datum = target.__data__ as FlameHierarchyFrame<FlameFrame> | undefined;
    if (!datum || !datum.data || datum.data.path === undefined) return;
    event.preventDefault();
    const bounds = wrap.getBoundingClientRect();
    // Clamp inside the wrap (which clips via overflow:hidden): the menu is
    // a fixed two-item block, so a size estimate is enough.
    menu = {
      x: Math.max(0, Math.min(event.clientX - bounds.left, wrap.clientWidth - MENU_WIDTH)),
      y: Math.max(0, Math.min(event.clientY - bounds.top, wrap.clientHeight - MENU_HEIGHT)),
      path: datum.data.path,
      name: datum.data.name,
      // "(all)" and "(other)" name no real disk entry — reveal them and
      // the engine could only answer with an error, so the item disables.
      synthetic: datum.data.name === "(all)" || datum.data.name === "(other)",
    };
  }

  function closeMenu(): void {
    menu = null;
  }

  function revealFromMenu(): void {
    if (!menu || menu.synthetic) return;
    onReveal(menu.path);
    closeMenu();
  }

  /// Task 18: the 加入集合 item — the NODE itself (directory or file, not
  /// its children) joins the collection; the parent view resolves the
  /// path → row id (file_id_of) and opens the shared picker.
  function addToCollectionFromMenu(): void {
    if (!menu || menu.synthetic) return;
    onAddToCollection(menu.path);
    closeMenu();
  }

  // ---------------- Chart lifecycle ----------------

  $effect(() => {
    // Depends on the `root` prop only: first run builds the chart, later
    // runs swap the datum (a drill or breadcrumb jump re-rooted by the
    // parent). `height(null)` makes the library re-derive the svg height
    // from the new tree's depth instead of keeping the old one.
    const data = toFlameFrame(root, root.size);
    if (!chart) {
      tooltip = flameTooltipModule.defaultFlamegraphTooltip();
      tooltip.html(tooltipHtml);
      chart = flamegraph()
        // The library defaults to 960px, which overflows a compare pane;
        // measure once up front, then let the ResizeObserver keep up.
        .width(wrap.clientWidth || FALLBACK_WIDTH)
        .cellHeight(CELL_HEIGHT)
        .transitionDuration(TRANSITION_MS)
        .minFrameSize(MIN_FRAME_PX)
        .tooltip(tooltip)
        .color(frameColor)
        .onClick((d: FlameHierarchyFrame<FlameFrame>) => onDrill(d.data.path));
      select(el).datum(data).call(chart);
      resizeObserver = new ResizeObserver(() => {
        clearTimeout(resizeTimer);
        resizeTimer = setTimeout(() => {
          if (!chart || !wrap) return;
          const width = wrap.clientWidth || FALLBACK_WIDTH;
          chart.width(width).update();
        }, 80);
      });
      resizeObserver.observe(wrap);
    } else {
      chart.height(null).update(data);
    }
  });

  /// Theme flips need a repaint — the fills are computed imperatively.
  function onThemeChange(): void {
    chart?.update();
  }

  darkQuery.addEventListener("change", onThemeChange);
  onDestroy(() => {
    darkQuery.removeEventListener("change", onThemeChange);
    clearTimeout(resizeTimer);
    resizeObserver?.disconnect();
    chart?.destroy();
    chart = null;
    // Belt-and-braces for the floating tooltip div: chart.destroy() tears
    // it down too (library contract), but if the chart was never mounted
    // (destroy early-returns) the div appended to <body> would leak — one
    // per compare-toggle otherwise.
    tooltip?.destroy();
    tooltip = null;
  });
</script>

<svelte:window
  onclick={closeMenu}
  oncontextmenu={onWindowContextMenu}
  onkeydown={(event) => {
    if (event.key === "Escape") closeMenu();
  }}
  onresize={closeMenu}
/>

<div class="flame-wrap" bind:this={wrap}>
  <div class="flame-chart" role="img" aria-label={$t("flame.chartLabel")} bind:this={el}></div>

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
        onclick={revealFromMenu}
      >
        {$t("flame.reveal")}
      </button>
      <button class="ctx-item" role="menuitem" onclick={addToCollectionFromMenu}>
        {$t("flame.addToCollection")}
      </button>
    </div>
  {/if}
</div>

<style>
  .flame-wrap {
    position: relative;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 8px;
    overflow: hidden;
  }

  .flame-chart {
    width: 100%;
  }

  /* d3 draws these nodes itself, so Svelte scoping can't reach them —
     :global from the component's own wrapper keeps the blast radius here. */

  /* Exact validated fills (the lib default 0.8 opacity would wash them
     toward the surface) and a theme-appropriate hairline frame gap. */
  .flame-wrap :global(.d3-flame-graph rect) {
    fill-opacity: 1;
  }

  .flame-wrap :global(.d3-flame-graph-label) {
    font-family: inherit;
  }

  @media (prefers-color-scheme: dark) {
    .flame-wrap :global(.d3-flame-graph rect) {
      stroke: rgb(255 255 255 / 0.22);
    }

    .flame-wrap :global(.d3-flame-graph-label) {
      color: var(--text);
    }
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
</style>
