/// <reference types="svelte" />
/// <reference types="vite/client" />

// d3-flame-graph ships no TypeScript definitions; this declares the subset
// of its API the flame chart uses (verified against dist/d3-flamegraph.es.js
// of the installed version). The hierarchy-node shape mirrors what
// d3-hierarchy's `hierarchy()` + `partition()` produce: `data` is OUR data
// object, `value` is the (zoom-mutable) laid-out value, and `parent` links
// up the tree.
declare module "d3-flame-graph" {
  /** One d3-hierarchy node wrapping a FlameFrame datum (see FlameChart). */
  export interface FlameHierarchyFrame<TData> {
    depth: number;
    parent: FlameHierarchyFrame<TData> | null;
    data: TData;
    highlight?: boolean;
  }

  export interface FlameGraphTooltip {
    show(event: MouseEvent, d: unknown): unknown;
    hide(): unknown;
    html<T>(fn: (d: FlameHierarchyFrame<T>) => string): FlameGraphTooltip;
    text<T>(fn: (d: FlameHierarchyFrame<T>) => string): FlameGraphTooltip;
    destroy(): void;
  }

  export interface FlameGraphChart {
    /** `select(el).datum(data).call(chart)` — the initial render. */
    (selection: unknown): unknown;
    width(px: number): FlameGraphChart;
    /** `null` lets the next update() recompute from the data's depth. */
    height(px: number | null): FlameGraphChart;
    minHeight(px: number): FlameGraphChart;
    cellHeight(px: number): FlameGraphChart;
    transitionDuration(ms: number): FlameGraphChart;
    minFrameSize(px: number): FlameGraphChart;
    color<T>(
      fn: (d: FlameHierarchyFrame<T>, originalColor: string) => string,
    ): FlameGraphChart;
    onClick<T>(fn: (d: FlameHierarchyFrame<T>) => void): FlameGraphChart;
    tooltip(tip: FlameGraphTooltip): FlameGraphChart;
    /** Swaps the datum (and reprocesses it) when `data` is given. */
    update(data?: unknown): FlameGraphChart;
    resetZoom(): FlameGraphChart;
    destroy(): FlameGraphChart;
  }

  /** `select(el).datum(data).call(chart)` — see FlameGraphChart. */
  export default function flamegraph(): FlameGraphChart;
  /** Namespace export; the floating-tooltip factory lives in here. */
  export const tooltip: {
    defaultFlamegraphTooltip(): FlameGraphTooltip;
  };
}
