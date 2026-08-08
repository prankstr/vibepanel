//! Mapping from keyboard layout language names to short display codes.
//!
//! Provides lookups for normalizing keyboard layout display across different
//! compositor backends:
//!
//! Compositor backends report full descriptions like `"English (US)"`. After
//! extracting the base language name (e.g., `"Swedish"`), use
//! [`short_code_from_language`] to get a 2-letter display code.

/// A single entry in the layout mapping table.
struct LayoutEntry {
    code: &'static str,
    language: &'static str,
}

/// Layout mapping table covering common keyboard-layout language names.
///
/// Sourced from vibepanel stargazer demographics, common Wayland desktop
/// layouts, and standard locale names. This table is intentionally kept small.
const LAYOUTS: &[LayoutEntry] = &[
    // Nordic
    LayoutEntry {
        code: "SE",
        language: "Swedish",
    },
    LayoutEntry {
        code: "NO",
        language: "Norwegian",
    },
    LayoutEntry {
        code: "DK",
        language: "Danish",
    },
    LayoutEntry {
        code: "FI",
        language: "Finnish",
    },
    // Western Europe
    LayoutEntry {
        code: "DE",
        language: "German",
    },
    LayoutEntry {
        code: "FR",
        language: "French",
    },
    LayoutEntry {
        code: "GB",
        language: "English",
    },
    LayoutEntry {
        code: "ES",
        language: "Spanish",
    },
    LayoutEntry {
        code: "IT",
        language: "Italian",
    },
    LayoutEntry {
        code: "PT",
        language: "Portuguese",
    },
    LayoutEntry {
        code: "NL",
        language: "Dutch",
    },
    LayoutEntry {
        code: "BE",
        language: "Belgian",
    },
    LayoutEntry {
        code: "CH",
        language: "Swiss",
    },
    LayoutEntry {
        code: "AT",
        language: "Austrian",
    },
    // Eastern Europe
    LayoutEntry {
        code: "PL",
        language: "Polish",
    },
    LayoutEntry {
        code: "CZ",
        language: "Czech",
    },
    LayoutEntry {
        code: "HU",
        language: "Hungarian",
    },
    LayoutEntry {
        code: "RO",
        language: "Romanian",
    },
    LayoutEntry {
        code: "UA",
        language: "Ukrainian",
    },
    LayoutEntry {
        code: "RU",
        language: "Russian",
    },
    // Americas
    LayoutEntry {
        code: "CA",
        language: "Canadian",
    },
    // Middle East
    LayoutEntry {
        code: "TR",
        language: "Turkish",
    },
    LayoutEntry {
        code: "IL",
        language: "Hebrew",
    },
    LayoutEntry {
        code: "AR",
        language: "Arabic",
    },
    // Asia
    LayoutEntry {
        code: "JP",
        language: "Japanese",
    },
    LayoutEntry {
        code: "KR",
        language: "Korean",
    },
    LayoutEntry {
        code: "CN",
        language: "Chinese",
    },
    LayoutEntry {
        code: "ID",
        language: "Indonesian",
    },
    // Central Asia
    LayoutEntry {
        code: "UZ",
        language: "Uzbek",
    },
];

/// Look up a short display code from an English language name (case-insensitive).
///
/// Callers should prefer parenthesized codes when available because language
/// names such as `"English"` do not uniquely identify a keyboard layout.
///
/// ```text
/// "Swedish" → Some("SE")
/// "German"  → Some("DE")
/// "Klingon" → None
/// ```
pub fn short_code_from_language(name: &str) -> Option<&'static str> {
    let name_lower = name.to_lowercase();
    LAYOUTS
        .iter()
        .find(|e| e.language.to_lowercase() == name_lower)
        .map(|e| e.code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_code_from_language() {
        assert_eq!(short_code_from_language("Swedish"), Some("SE"));
        assert_eq!(short_code_from_language("German"), Some("DE"));
        assert_eq!(short_code_from_language("Japanese"), Some("JP"));
        assert_eq!(short_code_from_language("Klingon"), None);
    }

    #[test]
    fn test_short_code_from_language_case_insensitive() {
        assert_eq!(short_code_from_language("swedish"), Some("SE"));
        assert_eq!(short_code_from_language("GERMAN"), Some("DE"));
    }

    #[test]
    fn test_all_entries_have_uppercase_codes() {
        for entry in LAYOUTS {
            assert_eq!(
                entry.code,
                entry.code.to_uppercase(),
                "layout code for '{}' should be uppercase",
                entry.language
            );
        }
    }

    #[test]
    fn test_all_entries_have_capitalized_language() {
        for entry in LAYOUTS {
            assert!(
                entry.language.starts_with(|c: char| c.is_uppercase()),
                "language '{}' should be capitalized",
                entry.language
            );
        }
    }
}
