<script lang="ts">
  // Sunburst mode: ECharts radial "cake tower" — the root at the center,
  // directories ring by ring outward. The empty arc inside a parent IS
  // that dir's direct-file share (dir-only data). Click = drill.
  //
  // Full-package import (not echarts/core + charts/...): the project
  // bundles with rolldown-vite, whose deep-path interop with echarts'
  // modular entry left a silently-blank canvas; the index entry is the
  // well-trodden path. Init errors surface ON the pane (we have no
  // devtools in the release webview) — see `fatal`.
  import { onDestroy } from "svelte";
  import * as echarts from "echarts";
  import type { SizeNode } from "../lib/types";

  let {
    root,
    onDrill,
    onHover,
  }: {
    root: SizeNode;
    onDrill: (path: string) => void;
    onReveal: (path: string) => void;
    onAddToCollection: (path: string) => void;
    /// Hover reporting for the parent's side panels: the hovered block's
    /// root-first chain (right panel) and its subdirectories sorted by
    /// size desc (left panel). Null on leave. The chart's rendering
    /// options are untouched by this — pure event tap.
    onHover?: (info: {
      chain: { name: string; size: number }[];
      children: { name: string; size: number }[];
    } | null) => void;
  } = $props();

  interface SunDatum {
    name: string;
    value: number;
    path: string;
    fileCount: number;
    pctOfParent: number;
    // Root-first ancestor chain INCLUDING this node (extra field —
    // ECharts ignores unknown data fields).
    chain: { name: string; size: number }[];
    itemStyle?: { color: string; borderColor: string; borderWidth: number };
    children?: SunDatum[];
  }

  function toSun(
    node: SizeNode,
    parentSize: number,
    ancestors: { name: string; size: number }[],
  ): SunDatum {
    const chain = [...ancestors, { name: node.name, size: node.size }];
    const datum: SunDatum = {
      name: node.name,
      value: node.size,
      path: node.path,
      fileCount: node.file_count,
      pctOfParent: parentSize > 0 ? (node.size / parentSize) * 100 : 100,
      chain,
    };
    if (node.children.length > 0) {
      datum.children = node.children.map((c) => toSun(c, node.size, chain));
    }
    return datum;
  }

  function countNodes(node: SizeNode): number {
    return 1 + node.children.reduce((s, c) => s + countNodes(c), 0);
  }

  const COLORS = {
    light: ["#bd7a30", "#2472a8", "#dfa262", "#4f9fd1", "#c98a45", "#3488bd"],
    dark: ["#96601a", "#1667a4", "#a06a25", "#24699c", "#9c6a20", "#1c72a8"],
  };

  let el: HTMLDivElement | undefined = $state();
  let fatal = $state<string | null>(null);
  let chart: echarts.ECharts | null = null;
  let resizeObserver: ResizeObserver | null = null;
  const darkQuery = window.matchMedia("(prefers-color-scheme: dark)");
  let isDark = $state(darkQuery.matches);

  function decorate(datum: SunDatum, depth: number): void {
    const colors = isDark ? COLORS.dark : COLORS.light;
    datum.itemStyle = {
      color: colors[depth % colors.length],
      borderColor: isDark ? "#1d2129" : "#ffffff",
      borderWidth: 1,
    };
    for (const child of datum.children ?? []) {
      decorate(child, depth + 1);
    }
  }

  $effect(() => {
    if (!el) return;
    try {
      if (!chart) {
        // Explicit dims at init: echarts on a zero-size container paints
        // nothing; the ResizeObserver corrects after layout settles.
        chart = echarts.init(el, null, {
          width: el.clientWidth || 600,
          height: el.clientHeight || 400,
        });
        chart.on("click", (params) => {
          const datum = params.data as SunDatum | undefined;
          if (datum && datum.path !== undefined) onDrill(datum.path);
        });
        // Hover tap for the side panels. Guarded by path: ECharts fires
        // mouseover per arc entry, and re-emitting the same arc on every
        // wiggle re-rendered the panels for nothing (hover-lag culprit #3).
        let lastHoverPath: string | null | undefined;
        chart.on("mouseover", (params) => {
          const datum = params.data as SunDatum | undefined;
          const path = datum?.path;
          if (path === lastHoverPath) return;
          lastHoverPath = path ?? null;
          if (!datum || path === undefined) {
            onHover?.(null);
            return;
          }
          // Children arrive engine-sorted (size desc); sort again so the
          // promoted-roots path is covered too.
          const children = [...(datum.children ?? [])].sort(
            (a, b) => b.value - a.value,
          );
          onHover?.({
            chain: datum.chain,
            children: children.map((c) => ({ name: c.name, size: c.value })),
          });
        });
        chart.on("globalout", () => {
          if (lastHoverPath === null) return;
          lastHoverPath = null;
          onHover?.(null);
        });
        resizeObserver = new ResizeObserver(() => {
          chart?.resize();
        });
        resizeObserver.observe(el);
      }
      // The virtual "(all)" root names no real directory and adds a
      // useless full-circle base ring — promote the scan roots straight
      // to the top level instead (each root gets its share of the ring).
      const promote =
        root.name === "(all)" && root.children.length > 0;
      const roots = promote
        ? root.children.map((c) => toSun(c, root.size, []))
        : [toSun(root, root.size, [])];
      for (const r of roots) {
        decorate(r, 0);
      }
      chart.setOption(
        {
          backgroundColor: "transparent",
          series: [
            {
              type: "sunburst",
              // No tooltip (hover-lag culprit #2): the side panels carry
              // richer info than the floating bubble did.
              tooltip: { show: false },
              // Ancestor highlight on hover (user-kept effect): the
              // hovered dir's whole lineage lights up, everything else
              // dims. Costs a canvas repaint per arc crossing — the other
              // hover-path optimizations (no tooltip, guarded panel
              // updates) keep the total feel smooth.
              emphasis: { focus: "ancestor" },
              // series.data is an ARRAY of roots — a bare object renders
              // as a single childless disc (the blank-cake bug).
              data: roots,
              radius: ["6%", "94%"],
              nodeClick: false, // we drill via our own click handler
              // No labels (user preference): on a radial map they rotate
              // with the arcs and read badly; the side panels carry names.
              label: { show: false },
            },
          ],
        },
        { notMerge: true },
      );
      fatal = null;
    } catch (err) {
      fatal = String(err);
    }
  });

  function onThemeChange(): void {
    isDark = darkQuery.matches;
  }

  darkQuery.addEventListener("change", onThemeChange);
  onDestroy(() => {
    darkQuery.removeEventListener("change", onThemeChange);
    resizeObserver?.disconnect();
    chart?.dispose();
    chart = null;
  });
</script>

<!-- flame-wrap class: FlameView's pane layout keys on it (flex:1 +
     min-height:0). The fixed min pixel height is diagnosis belt: even if
     every flex rule vanished, the chart still gets a drawable box. -->
<div class="flame-wrap sunburst-wrap" bind:this={el}>
  {#if fatal}
    <div class="fatal" role="alert">sunburst: {fatal}</div>
  {:else if countNodes(root) <= 1}
    <div class="hintline">sunburst: {countNodes(root)} node</div>
  {/if}
</div>

<style>
  .sunburst-wrap {
    position: relative;
    width: 100%;
    height: 100%;
    min-height: 320px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 12px;
  }

  .fatal {
    position: absolute;
    inset: 12px;
    z-index: 5;
    color: #d33;
    font-size: 12px;
    word-break: break-all;
  }

  .hintline {
    position: absolute;
    inset: 0;
    z-index: 5;
    display: grid;
    place-items: center;
    color: var(--text);
    opacity: 0.6;
    font-size: 12px;
    pointer-events: none;
  }
</style>
