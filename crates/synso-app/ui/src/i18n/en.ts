// English UI strings. The shape of this object IS the i18n contract:
// zh.ts must implement exactly the same keys (enforced by
// `npm run check:i18n` at CI time and by the Dict type at editor time).

const en = {
  app: {
    title: "Synso",
  },
  lang: {
    auto: "Auto (system)",
    zh: "中文",
    en: "English",
  },
  a11y: {
    langSelect: "Language / 语言",
    scanProgress: "Scan progress",
    dismiss: "Dismiss",
    quickInput: "Quick search input",
    modifierChips: "Recognized query modifiers",
    resultsList: "Search results",
    dedupProgress: "Duplicate analysis progress",
    prevPage: "Previous page",
    nextPage: "Next page",
    confirmDialog: "Delete confirmation",
    shortcutInput: "Shortcut combo input",
    exclusionsInput: "Exclusion patterns input",
    volumesList: "Index volumes",
  },
  dashboard: {
    files: "Files",
    dirs: "Directories",
    index: "Index status",
    warm: "Ready",
    cold: "Loading",
    interned: "Index memory",
    scan: "Start scan",
    rescan: "Rescan",
    scanning: "Scanning…",
    scanFailed: "Scan failed",
    quickSearch: "Quick search",
  },
  tabs: {
    overview: "Overview",
    dedup: "Duplicates",
    flame: "Capacity map",
    settings: "Settings",
  },
  dedup: {
    start: "Start analysis",
    rerun: "Re-analyze",
    phase: {
      idle: "Not analyzed yet",
      grouping: "Grouping by size",
      hashing_first: "Quick hashing (first 4 KiB)",
      hashing_full: "Full hashing",
      done: "Analysis complete",
    },
    empty: "No duplicate files found",
    statsGroups: "groups",
    wasted: "recoverable",
    members: "files",
    stale:
      "Groups from the last analysis — they refresh when the new one finishes.",
    skippedPrefix: "Skipped",
    filesUnit: "files",
    deletedPrefix: "Deleted",
    keepNewest: "Keep newest",
    keepOldest: "Keep oldest",
    keepNewestHint:
      "Delete the others and keep only the most recently modified file.",
    keepOldestHint:
      "Delete the others and keep only the oldest-modified file.",
    manualDelete: "Delete selected",
    confirmTitle: "Delete these files?",
    confirmTrash:
      "Files move to the Recycle Bin — they can be restored from there.",
    confirmDelete: "Delete",
    confirmCancel: "Cancel",
    confirmAllHint:
      "Each group must keep at least one file — uncheck something first.",
    deleteFailed: "Deletion failed",
  },
  wizard: {
    title: "Welcome to Synso",
    subtitle:
      "A few quick choices before the first scan — everything here can be changed later in settings.",
    stepLang: "Language",
    stepVolumes: "Volumes",
    stepExclusions: "Exclusions",
    stepAutostart: "Launch at startup",
    langHint: "Pick the interface language. You can switch anytime from the dashboard.",
    volumesHint:
      "All fixed local disks are indexed by default. Uncheck a disk to leave it out of the index.",
    volumesEmpty: "Select at least one volume to scan.",
    exclusionsHint:
      "One glob pattern per line; matching files and directories stay out of the index. For example:",
    autostartLabel: "Launch Synso when the computer starts",
    autostartHint:
      "On by default: after login Synso runs in the background and keeps the index up to date. Flip the switch (or change it later in settings) to turn this off.",
    back: "Back",
    next: "Next",
    begin: "Start scanning",
    scanning: "Building the index…",
    retry: "Retry",
  },
  quick: {
    placeholder: "Search files… (ext: size: path:)",
    hint: "Type to search — pinyin initials work too (bg finds 报告.docx).",
    noResults: "No results",
    warming: "The index is warming up — results appear in a moment.",
    results: "results",
    hints: "Enter open · Ctrl+Enter reveal · Esc close",
  },
  toast: {
    shortcut:
      "The global shortcut is unavailable (taken or invalid). Use the Quick search button on the dashboard instead.",
  },
  flame: {
    compare: "Compare mode",
    refresh: "Refresh",
    depth: "Depth",
    depthLess: "Fewer layers",
    depthMore: "More layers",
    noDirs: "No subdirectories in this scope",
    resetZoom: "Reset zoom",
    pathPanel: "Directory path",
    pathHint: "Hover a block in the chart to see its full path and size here",
    childrenPanel: "Subdirectories",
    childrenHint: "Hover a block to list its subdirectories by size here",
    childrenEmpty: "This directory has no subdirectories",
    loading: "Loading…",
    missing: "Path not found",
    loadFailed: "Failed to load",
    rootPickerPlaceholder: "Index path — Enter to jump",
    rootPickerLabel: "Root path picker",
    crumbsLabel: "Root breadcrumb",
    chartLabel: "Directory size capacity map",
    menuLabel: "Node menu",
    reveal: "Show in File Explorer",
    revealUnavailable: "Aggregated nodes have no disk location",
    aggregateNoDrill:
      "Aggregated node — the (other) bucket has no single path to drill into.",
    filesLine: "{n} files",
    ofParentLine: "{pct}% of parent",
    used: "Used",
    total: "of",
    unindexed: "Unindexed",
    freeSpace: "Free space",
  },
  settings: {
    langTitle: "Language",
    autostartTitle: "Launch at startup",
    autostartHint:
      "Applies immediately: the switch updates the OS launch-at-startup registration live.",
    shortcutTitle: "Global shortcut",
    shortcutHint:
      "The quick search window's hotkey, e.g. Ctrl+Shift+Space. Apply re-registers it right away; if another app owns the combo, the old one is kept and the error is shown.",
    apply: "Apply",
    applied: "Applied",
    shortcutFailed: "Shortcut unavailable",
    indexTitle: "Index scope",
    customHint:
      "Add any folder as a scan root to keep the test scope small during development — uncheck every disk and keep only custom paths for pinpoint indexing. Remove them and re-check disks to return to full-volume indexing once stable.",
    customRoots: "Custom scan paths",
    addFolder: "Add folder…",
    removeRoot: "Remove",
    save: "Save",
    saved: "Saved — scope changes take effect after a rescan.",
    rescanStarted: "Rescan started — track it on the Overview tab.",
  },
};

export default en;
export type Dict = typeof en;
