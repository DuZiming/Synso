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
};

export default en;
export type Dict = typeof en;
