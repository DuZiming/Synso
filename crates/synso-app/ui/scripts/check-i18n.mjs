// Verifies zh.ts and en.ts carry EXACTLY the same key set (recursively).
// Exits 1 on any mismatch, so CI catches a missing translation. Reads the
// .ts dictionaries directly via Node's built-in type stripping.

import en from "../src/i18n/en.ts";
import zh from "../src/i18n/zh.ts";

const leafPaths = (value, prefix = "") =>
  Object.entries(value).flatMap(([key, child]) =>
    child !== null && typeof child === "object"
      ? leafPaths(child, `${prefix}${key}.`)
      : [`${prefix}${key}`],
  );

const enKeys = leafPaths(en);
const zhKeys = leafPaths(zh);
const enSet = new Set(enKeys);
const zhSet = new Set(zhKeys);

let failed = false;

const missingInZh = enKeys.filter((key) => !zhSet.has(key));
if (missingInZh.length > 0) {
  console.error(`Keys missing in zh.ts:\n  ${missingInZh.join("\n  ")}`);
  failed = true;
}

const missingInEn = zhKeys.filter((key) => !enSet.has(key));
if (missingInEn.length > 0) {
  console.error(`Keys missing in en.ts:\n  ${missingInEn.join("\n  ")}`);
  failed = true;
}

// Duplicate keys collapse in the sets above; exact counts catch them.
if (enKeys.length !== enSet.size || zhKeys.length !== zhSet.size) {
  console.error("Duplicate keys detected in a dictionary.");
  failed = true;
}

// Every leaf must be a non-empty string.
for (const [name, dict] of [
  ["en", en],
  ["zh", zh],
]) {
  for (const key of leafPaths(dict)) {
    const value = key.split(".").reduce((node, part) => node?.[part], dict);
    if (typeof value !== "string" || value.length === 0) {
      console.error(
        `Key ${name}:${key} must be a non-empty string, got ${JSON.stringify(value)}`,
      );
      failed = true;
    }
  }
}

if (failed) process.exit(1);
console.log(`i18n OK: ${enKeys.length} keys, zh/en key sets identical.`);
