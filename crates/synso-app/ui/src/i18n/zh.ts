// Chinese UI strings — must mirror en.ts key for key (Dict type here,
// `npm run check:i18n` in CI).

import type { Dict } from "./en";

const zh: Dict = {
  app: {
    title: "Synso",
  },
  lang: {
    auto: "跟随系统",
    zh: "中文",
    en: "English",
  },
  dashboard: {
    files: "文件",
    dirs: "目录",
    index: "索引状态",
    warm: "就绪",
    cold: "加载中",
    interned: "索引内存",
    scan: "开始扫描",
    rescan: "重新扫描",
    scanning: "扫描中…",
    scanFailed: "扫描失败",
  },
};

export default zh;
