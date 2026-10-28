// Settings-editing helpers shared by the first-run wizard (Task 14) and
// the settings view (Task 19) — one implementation, identical semantics
// by construction.

/** One trimmed glob per line; blanks and duplicates dropped. The engine's
 * `Settings.exclusions` spelling (a list of patterns). */
export function parseExclusions(text: string): string[] {
  const seen = new Set<string>();
  for (const line of text.split(/\r\n|\r|\n/)) {
    const pattern = line.trim();
    if (pattern !== "") seen.add(pattern);
  }
  return [...seen];
}

/** The volume-picker state for a fresh mount: every listed volume checked
 * (empty `Settings.volumes` means "all fixed volumes" — the same default
 * the wizard teaches). */
export function volumesChecked(
  volumes: { path: string }[],
  saved: string[],
): Record<string, boolean> {
  if (saved.length === 0) {
    return Object.fromEntries(volumes.map((v) => [v.path, true]));
  }
  const kept = new Set(saved);
  return Object.fromEntries(volumes.map((v) => [v.path, kept.has(v.path)]));
}

/** The persisted form of the picker state: all checked → `[]` (the
 * "default = all" spelling), else the checked paths in list order. */
export function volumesSaved(
  volumes: { path: string }[],
  checked: Record<string, boolean>,
): string[] {
  const all = volumes.length > 0 && volumes.every((v) => checked[v.path]);
  if (all) return [];
  return volumes.filter((v) => checked[v.path]).map((v) => v.path);
}
