//! Pinyin keys for Han-named files: the two transliteration strings that let
//! Latin queries find them (「bg」 → 「报告.docx」).
//!
//! [`pinyin_keys`] is the single generation choke point: the store computes
//! both keys from the name at upsert time (`files.py_full` / `files.py_init`),
//! the hot index loads them as parallel arenas beside the names, and search
//! substring-matches them after the literal name. Callers never compute or
//! pass pinyin themselves.
//!
//! Key shape — both keys are built per character:
//! - Han character → its first reading: the full syllable in `py_full`
//!   ("bao"), the first letter only in `py_init` ("b"). Multi-reading
//!   characters take the first reading.
//! - Anything else passes through lowercased into BOTH keys (ASCII letters
//!   lowercased; digits, punctuation and dots as-is), so 「文件ABC」 maps to
//!   ("wenjianabc", "wjabc") and extensions stay searchable in both keys.

use pinyin::ToPinyin;

/// Computes `(py_full, py_init)` for one file name (see module docs).
pub fn pinyin_keys(name: &str) -> (String, String) {
    let mut py_full = String::with_capacity(name.len());
    let mut py_init = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch.to_pinyin() {
            // Han: `to_pinyin` on a char yields the first reading.
            Some(py) => {
                py_full.push_str(py.plain());
                py_init.push_str(py.first_letter());
            }
            // Anything else passes through lowercased into both keys
            // (`to_lowercase` can expand, e.g. 'İ' -> "i̇", so iterate it).
            None => {
                for lower in ch.to_lowercase() {
                    py_full.push(lower);
                    py_init.push(lower);
                }
            }
        }
    }
    (py_full, py_init)
}

#[cfg(test)]
mod tests {
    use super::pinyin_keys;

    #[test]
    fn pinyin_keys_basic() {
        // Han expand to their first readings; the extension passes through.
        assert_eq!(
            pinyin_keys("报告.docx"),
            ("baogao.docx".to_string(), "bg.docx".to_string())
        );
        // Non-Han characters pass through lowercased in BOTH keys.
        assert_eq!(
            pinyin_keys("文件ABC"),
            ("wenjianabc".to_string(), "wjabc".to_string())
        );
        // Pure-ASCII names degenerate to their lowercase spelling twice.
        assert_eq!(
            pinyin_keys("README.MD"),
            ("readme.md".to_string(), "readme.md".to_string())
        );
        // Digits and punctuation pass through untouched in both keys.
        assert_eq!(
            pinyin_keys("2024年计划.md"),
            ("2024nianjihua.md".to_string(), "2024njh.md".to_string())
        );
        assert_eq!(pinyin_keys(""), (String::new(), String::new()));
    }
}
