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
};

export default en;
export type Dict = typeof en;
