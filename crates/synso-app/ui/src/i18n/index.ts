// Tiny typed i18n: nested dictionaries + a dotted-key translator store.
// No i18n library — the dictionaries are plain TS objects, `zh` is typed
// against `en`'s shape, and `npm run check:i18n` re-checks the key sets
// outside the compiler. Switching the language is live: `t` is a derived
// store, so every `$t(...)` in the templates re-renders on change.

import { derived, writable } from "svelte/store";
import en, { type Dict } from "./en";
import zh from "./zh";
import { settingsGet, settingsSet } from "../lib/tauri";

export type Locale = "zh" | "en";

/** What `Settings.lang` stores: a concrete locale or "follow the OS". */
export type LangSetting = Locale | "auto";

/** Dot-separated keys of the dictionary, computed from its shape. */
type Paths<T> = T extends object
  ? { [K in keyof T & string]: `${K}` | `${K}.${Paths<T[K]>}` }[keyof T & string]
  : never;
export type I18nKey = Paths<Dict>;

const dicts: Record<Locale, Dict> = { en, zh };

/** The persisted language preference (mirrors `Settings.lang`). */
export const langSetting = writable<LangSetting>("auto");

function resolve(setting: LangSetting): Locale {
  if (setting === "zh" || setting === "en") return setting;
  // "auto": follow the OS locale, Chinese when it starts with "zh".
  const nav = typeof navigator === "undefined" ? "" : navigator.language;
  return nav.toLowerCase().startsWith("zh") ? "zh" : "en";
}

function lookup(locale: Locale, key: I18nKey): string | undefined {
  let node: unknown = dicts[locale];
  for (const part of key.split(".")) {
    if (node === null || typeof node !== "object") return undefined;
    node = (node as Record<string, unknown>)[part];
  }
  return typeof node === "string" ? node : undefined;
}

/** Reactive translator: `$t("dashboard.files")`. Falls back to English,
 * then to the key itself, so a missing translation can never crash. */
export const t = derived(langSetting, ($setting) => {
  const locale = resolve($setting);
  return (key: I18nKey): string => lookup(locale, key) ?? lookup("en", key) ?? key;
});

/** Loads the persisted preference into the store (call once at startup).
 *
 * Retries briefly: the engine state is managed in the Tauri setup hook,
 * which runs AFTER the config windows exist — a page that mounts fast can
 * see "state not managed" rejections for a few hundred ms (surfaced by the
 * Task 15 dev run: without the retry, the wizard's view decision raced the
 * backend and a fresh install could land straight on the dashboard). A
 * genuinely dead backend exhausts the retries; language is a preference,
 * so the caller's fallback (and the store's default) apply instead. */
export async function initI18n(): Promise<void> {
  for (let attempt = 0; attempt < 15; attempt++) {
    try {
      const settings = await settingsGet();
      langSetting.set(
        settings.lang === "zh" || settings.lang === "en" ? settings.lang : "auto",
      );
      return;
    } catch {
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
  }
}

/** Switches the language live AND persists the choice via settings_set. */
export async function changeLang(setting: LangSetting): Promise<void> {
  langSetting.set(setting);
  const settings = await settingsGet();
  await settingsSet({ ...settings, lang: setting });
}
