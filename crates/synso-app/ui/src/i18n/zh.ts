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
  a11y: {
    langSelect: "Language / 语言",
    scanProgress: "扫描进度",
    dismiss: "关闭",
    quickInput: "快速搜索输入框",
    modifierChips: "已识别的查询修饰符",
    resultsList: "搜索结果",
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
    quickSearch: "快速搜索",
  },
  wizard: {
    title: "欢迎使用 Synso",
    subtitle: "首次扫描前先完成几项选择——之后随时可以在设置中修改。",
    stepLang: "语言",
    stepVolumes: "索引卷",
    stepExclusions: "排除规则",
    stepAutostart: "开机自启",
    langHint: "选择界面语言，之后可随时在仪表盘上切换。",
    volumesHint: "默认索引所有固定的本地磁盘；取消勾选即可跳过对应磁盘。",
    volumesEmpty: "请至少选择一个要扫描的卷。",
    exclusionsHint: "每行一条通配符规则，匹配的文件与目录不会进入索引。例如：",
    autostartLabel: "开机时自动启动 Synso",
    autostartHint:
      "默认开启：登录系统后 Synso 会在后台自动运行，保持索引实时更新；用这个开关（或之后在设置里）即可一键关闭。",
    back: "上一步",
    next: "下一步",
    begin: "开始扫描",
    scanning: "正在建立索引…",
    retry: "重试",
  },
  quick: {
    placeholder: "搜索文件…（ext: size: path: coll:）",
    hint: "输入即搜，支持拼音首字母（如 bg 找到“报告.docx”）。",
    noResults: "无结果",
    warming: "索引正在预热，结果马上就好。",
    results: "项结果",
    hints: "回车打开 · Ctrl+回车 打开所在文件夹 · Esc 关闭",
  },
  toast: {
    shortcut:
      "全局快捷键不可用（被占用或无效），可改用仪表盘上的“快速搜索”按钮打开搜索窗。",
  },
};

export default zh;
