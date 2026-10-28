// Shared humanized-format helpers, extracted from the quick window
// (Task 16) so the dashboard's dedup view formats sizes and dates
// exactly like search results do.

/** 1024-based humanized byte size: "512 B", "2.0 KB", "1.5 MB"… */
export function humanBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"] as const;
  let value = bytes;
  let unit = -1;
  do {
    value /= 1024;
    unit++;
  } while (value >= 1024 && unit < units.length - 1);
  return `${value >= 100 ? value.toFixed(0) : value.toFixed(1)} ${units[unit]}`;
}

const timeFormat = new Intl.DateTimeFormat(undefined, {
  dateStyle: "short",
  timeStyle: "short",
});

/** Short localized date-time for a unix-epoch seconds stamp. */
export function formatTime(unixSecs: number): string {
  return timeFormat.format(new Date(unixSecs * 1000));
}
