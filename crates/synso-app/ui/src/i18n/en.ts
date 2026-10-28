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
    flame: "Flame chart",
    collections: "Collections",
    comingSoon: "Coming soon",
  },
  dedup: {
    start: "Start analysis",
    boost: "Boost",
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
    placeholder: "Search files… (ext: size: path: coll:)",
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
};

export default en;
export type Dict = typeof en;
