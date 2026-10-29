<script lang="ts">
  // The flame chart: ECharts radial "cake tower" — the root at the center,
  // directories ring by ring outward. The empty arc inside a parent IS
  // that dir's direct-file share (dir-only data). Click = drill.
  //
  // Hover feeds the RIGHT-SIDE PATH PANEL (user requirement): the full
  // ancestor chain from the scan root down to the hovered directory,
  // each level with name + size — the radial map answers "where", the
  // panel answers "what exactly and how big".
  import { onDestroy } from "svelte";
  import * as echarts from "echarts";
  import { t } from "../i18n";
  import { humanBytes } from "../lib/format";
  import type { SizeNode } from "../lib/types";

  let {
    root,
    onDrill,
  }: {
    root: SizeNode;
    onDrill: (path: string) => void;
    onReveal: (path: string) => void;
    onAddToCollection: (path: string) => void;
  } = $props();

  /// One level of the hover path panel.
  interface ChainLevel {
    name: string;
    size: number;
    fileCount: number;
    pctOfParent: number;
  }

  interface SunDatum {
    name: string;
    value: number;
    path: string;
    // Root-first ancestor chain INCLUDING this node (the panel's data).
    chain: ChainLevel[];
    itemStyle?: { color: string; borderColor: string; borderWidth: number };
    children?: SunDatum[];
  }

  function toSun(node: SizeNode, ancestors: ChainLevel[]): SunDatum {
    const level: ChainLevel = {
      name: node.name,
      size: node.size,
      fileCount: node.file_count,
      pctOfParent:
        ancestors.length > 0 && ancestors.at(-1)!.size > 0
          ? (node.size / ancestors.at(-1)!.size) * 100
          : 100,
    };
    const chain = [...ancestors, level];
    const datum: SunDatum = {
      name: node.name,
      value: node.size,
      path: node.path,
      chain,
    };
    if (node.children.length > 0) {
      datum.children = node.children.map((c) => toSun(c, chain));
    }
    return datum;
  }

  const COLORS = {
    light: ["#bd7a30", "#2472a8", "#dfa262", "#4f9fd1", "#c98a45", "#3488bd"],
    dark: ["#96601a", "#1667a4", "#a06a25", "#24699c", "#9c6a20", "#1c72a8"],
  };

  let el: HTMLDivElement | undefined = $state();
  let fatal = $state<string | null>(null);
  let hoverChain = $state<ChainLevel[] | null>(null);
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
        chart.on("mouseover", (params) => {
          const datum = params.data as SunDatum | undefined;
          hoverChain = datum?.chain ?? null;
        });
        chart.on("globalout", () => {
          hoverChain = null;
        });
        resizeObserver = new ResizeObserver(() => {
          chart?.resize();
        });
        resizeObserver.observe(el);
      }
      const data = toSun(root, []);
      decorate(data, 0);
      chart.setOption(
        {
          backgroundColor: "transparent",
          series: [
            {
              type: "sunburst",
              // series.data is an ARRAY of roots — a bare object renders
              // as a single childless disc.
              data: [data],
              radius: ["6%", "88%"],
              // Right side of the pane is the path panel's space.
              center: ["38%", "50%"],
              nodeClick: false, // we drill via our own click handler
              // No labels (user preference): on a radial map they rotate
              // with the arcs and read badly; the panel carries names.
              label: { show: false },
              emphasis: { focus: "none" },
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
     min-height:0). -->
<div class="flame-wrap sunburst-wrap" bind:this={el}>
  {#if fatal}
    <div class="fatal" role="alert">sunburst: {fatal}</div>
  {/if}

  {#if hoverChain && hoverChain.length > 0}
    <aside class="path-panel" aria-label={$t("flame.pathPanel")}>
      <h3>{$t("flame.pathPanel")}</h3>
      <ol>
        {#each hoverChain as level, i (i)}
          <li class:current={i === hoverChain.length - 1}>
            <span class="lvl-name" title={level.name}>{level.name}</span>
            <span class="lvl-size">{humanBytes(level.size)}</span>
            {#if i > 0}
              <span class="lvl-pct">
                {$t("flame.ofParentLine").replace("{pct}", level.pctOfParent.toFixed(1))}
              </span>
            {/if}
          </li>
        {/each}
      </ol>
    </aside>
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

  .path-panel {
    position: absolute;
    top: 10px;
    right: 10px;
    bottom: 10px;
    width: 34%;
    min-width: 180px;
    max-width: 300px;
    z-index: 6;
    overflow-y: auto;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 10px 12px;
    font-size: 12px;
    box-shadow: 0 4px 18px rgb(0 0 0 / 0.12);
    pointer-events: none; /* hover must keep reaching the chart */
  }

  .path-panel h3 {
    margin: 0 0 6px;
    font-size: 12px;
    font-weight: 600;
    color: var(--text);
    opacity: 0.75;
  }

  .path-panel ol {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }

  .path-panel li {
    display: flex;
    align-items: baseline;
    gap: 6px;
    flex-wrap: wrap;
    color: var(--text);
    padding: 2px 4px;
    border-radius: 4px;
  }

  .path-panel li.current {
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
  }

  .lvl-pct {
    flex-basis: 100%;
    font-size: 10.5px;
    opacity: 0.55;
    margin-top: -1px;
  }
</style>
