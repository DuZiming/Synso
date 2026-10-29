<script lang="ts">
  // Sunburst mode: ECharts radial "cake tower" — the root at the center,
  // directories ring by ring outward, each parent's children centered in
  // its arc. The empty arc inside a parent IS that dir's direct-file share
  // (dir-only data), the radial twin of the mountain chart's taper.
  // Click = drill (refetch via the parent); the tooltip is ECharts' own,
  // content-matched with the other modes.
  import { onDestroy } from "svelte";
  import { get } from "svelte/store";
  import * as echarts from "echarts/core";
  import { SunburstChart } from "echarts/charts";
  import { TooltipComponent } from "echarts/components";
  import { CanvasRenderer } from "echarts/renderers";
  import { t } from "../i18n";
  import { humanBytes } from "../lib/format";
  import type { SizeNode } from "../lib/types";

  echarts.use([SunburstChart, TooltipComponent, CanvasRenderer]);

  let {
    root,
    onDrill,
  }: {
    root: SizeNode;
    onDrill: (path: string) => void;
    onReveal: (path: string) => void;
    onAddToCollection: (path: string) => void;
  } = $props();

  interface SunDatum {
    name: string;
    value: number;
    path: string;
    fileCount: number;
    pctOfParent: number;
    itemStyle?: { color: string; borderColor: string; borderWidth: number };
    children?: SunDatum[];
  }

  function toSun(node: SizeNode, parentSize: number): SunDatum {
    const datum: SunDatum = {
      name: node.name,
      value: node.size,
      path: node.path,
      fileCount: node.file_count,
      pctOfParent: parentSize > 0 ? (node.size / parentSize) * 100 : 100,
    };
    if (node.children.length > 0) {
      datum.children = node.children.map((c) => toSun(c, node.size));
    }
    return datum;
  }

  // Per-depth warm/cool families (the app's validated palette) + a
  // theme-matched hairline between arcs (canvas cannot read CSS vars).
  const COLORS = {
    light: ["#bd7a30", "#2472a8", "#dfa262", "#4f9fd1", "#c98a45", "#3488bd"],
    dark: ["#96601a", "#1667a4", "#a06a25", "#24699c", "#9c6a20", "#1c72a8"],
  };

  let el: HTMLDivElement | undefined = $state();
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
    if (!chart) {
      // Explicit dims at init: echarts on a zero-size container paints
      // nothing (it logs "Can't get DOM width or height") — the layout
      // may not have settled when this first runs, so fall back to the
      // pane's box and let the ResizeObserver correct it right after.
      chart = echarts.init(el, null, {
        width: el.clientWidth || 600,
        height: el.clientHeight || 400,
      });
      chart.on("click", (params) => {
        const datum = params.data as SunDatum | undefined;
        if (datum && datum.path !== undefined) onDrill(datum.path);
      });
      resizeObserver = new ResizeObserver(() => {
        chart?.resize();
      });
      resizeObserver.observe(el);
    }
    const data = toSun(root, root.size);
    decorate(data, 0);
    chart.setOption(
      {
        backgroundColor: "transparent",
        tooltip: {
          formatter: (params: unknown) => {
            const d = (params as { data: SunDatum }).data;
            const files = get(t)("flame.filesLine").replace(
              "{n}",
              d.fileCount.toLocaleString(),
            );
            const share = get(t)("flame.ofParentLine").replace(
              "{pct}",
              d.pctOfParent.toFixed(1),
            );
            return `<b>${d.name}</b><br>${humanBytes(d.value)}<br>${files} · ${share}`;
          },
        },
        series: [
          {
            type: "sunburst",
            data,
            radius: ["8%", "92%"],
            nodeClick: false, // we drill via our own click handler
            label: {
              color: "#ffffff",
              textBorderColor: isDark ? "#1d2129" : "#ffffff",
              textBorderWidth: 2,
              minAngle: 8,
            },
            emphasis: { focus: "ancestor" },
          },
        ],
      },
      { notMerge: true },
    );
  });

  function onThemeChange(): void {
    isDark = darkQuery.matches; // re-decorates via the $effect dependency
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
     min-height:0) — a differently-named root would collapse to zero
     height in the flex column and echarts would paint nothing. -->
<div class="flame-wrap sunburst-wrap" bind:this={el}></div>

<style>
  .sunburst-wrap {
    /* All shared chrome (background/border/radius/size) comes from
       .flame-wrap's FlameView rule + this component's own copy below. */
    position: relative;
    width: 100%;
    height: 100%;
    min-height: 160px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 12px;
  }
</style>
