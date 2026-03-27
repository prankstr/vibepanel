//! Bidirectional mapping between XKB layout codes and display names.
//!
//! Provides lookups for normalizing keyboard layout display across different
//! compositor backends:
//!
//! - **Sway / Hyprland / Niri** report full descriptions like `"English (US)"`.
//!   After extracting the base language name (e.g., `"Swedish"`), use
//!   [`short_code_from_language`] to get a 2-letter display code.
//!
//! - **MangoWC / DWL** report raw XKB codes like `"swe"` or `"us"`.
//!   Use [`short_code_from_xkb`] to get a normalized 2-letter display code,
//!   and [`language_from_xkb`] to get a human-readable name for tooltips.

/// A single entry in the layout mapping table.
struct LayoutEntry {
    xkb: &'static str,
    code: &'static str,
    language: &'static str,
}

/// Layout mapping table covering the most common XKB layouts.
///
/// Sourced from vibepanel stargazer demographics, common Wayland desktop
/// layouts, and the XKB rules database. See `evdev.xml` for the full list
/// of XKB layouts — this table is intentionally kept small.
const LAYOUTS: &[LayoutEntry] = &[
    // Nordic
    LayoutEntry {
        xkb: "se",
        code: "SE",
        language: "Swedish",
    },
    LayoutEntry {
        xkb: "swe",
        code: "SE",
        language: "Swedish",
    },
    LayoutEntry {
        xkb: "no",
        code: "NO",
        language: "Norwegian",
    },
    LayoutEntry {
        xkb: "dk",
        code: "DK",
        language: "Danish",
    },
    LayoutEntry {
        xkb: "fi",
        code: "FI",
        language: "Finnish",
    },
    // Western Europe
    LayoutEntry {
        xkb: "de",
        code: "DE",
        language: "German",
    },
    LayoutEntry {
        xkb: "fr",
        code: "FR",
        language: "French",
    },
    LayoutEntry {
        xkb: "gb",
        code: "GB",
        language: "English",
    },
    LayoutEntry {
        xkb: "us",
        code: "US",
        language: "English",
    },
    LayoutEntry {
        xkb: "es",
        code: "ES",
        language: "Spanish",
    },
    LayoutEntry {
        xkb: "it",
        code: "IT",
        language: "Italian",
    },
    LayoutEntry {
        xkb: "pt",
        code: "PT",
        language: "Portuguese",
    },
    LayoutEntry {
        xkb: "nl",
        code: "NL",
        language: "Dutch",
    },
    LayoutEntry {
        xkb: "be",
        code: "BE",
        language: "Belgian",
    },
    LayoutEntry {
        xkb: "ch",
        code: "CH",
        language: "Swiss",
    },
    LayoutEntry {
        xkb: "at",
        code: "AT",
        language: "Austrian",
    },
    // Eastern Europe
    LayoutEntry {
        xkb: "pl",
        code: "PL",
        language: "Polish",
    },
    LayoutEntry {
        xkb: "cz",
        code: "CZ",
        language: "Czech",
    },
    LayoutEntry {
        xkb: "hu",
        code: "HU",
        language: "Hungarian",
    },
    LayoutEntry {
        xkb: "ro",
        code: "RO",
        language: "Romanian",
    },
    LayoutEntry {
        xkb: "ua",
        code: "UA",
        language: "Ukrainian",
    },
    LayoutEntry {
        xkb: "ru",
        code: "RU",
        language: "Russian",
    },
    // Americas
    LayoutEntry {
        xkb: "br",
        code: "BR",
        language: "Portuguese",
    },
    LayoutEntry {
        xkb: "latam",
        code: "LA",
        language: "Spanish",
    },
    LayoutEntry {
        xkb: "ca",
        code: "CA",
        language: "Canadian",
    },
    // Middle East
    LayoutEntry {
        xkb: "tr",
        code: "TR",
        language: "Turkish",
    },
    LayoutEntry {
        xkb: "il",
        code: "IL",
        language: "Hebrew",
    },
    LayoutEntry {
        xkb: "ara",
        code: "AR",
        language: "Arabic",
    },
    // Asia
    LayoutEntry {
        xkb: "jp",
        code: "JP",
        language: "Japanese",
    },
    LayoutEntry {
        xkb: "kr",
        code: "KR",
        language: "Korean",
    },
    LayoutEntry {
        xkb: "cn",
        code: "CN",
        language: "Chinese",
    },
    LayoutEntry {
        xkb: "id",
        code: "ID",
        language: "Indonesian",
    },
    // Central Asia
    LayoutEntry {
        xkb: "uz",
        code: "UZ",
        language: "Uzbek",
    },
];

/// Look up a short display code from an XKB layout code.
///
/// ```text
/// "swe" → Some("SE")
/// "us"  → Some("US")
/// "xyz" → None
/// ```
pub fn short_code_from_xkb(xkb: &str) -> Option<&'static str> {
    let xkb_lower = xkb.to_lowercase();
    LAYOUTS.iter().find(|e| e.xkb == xkb_lower).map(|e| e.code)
}

/// Look up a short display code from an English language name (case-insensitive).
///
/// Returns the first match — for ambiguous names like `"English"` (which maps
/// to both `us` and `gb`), callers should prefer parenthesized codes when available.
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

/// Look up a human-readable language name from an XKB layout code.
///
/// ```text
/// "swe" → Some("Swedish")
/// "us"  → Some("English")
/// "xyz" → None
/// ```
pub fn language_from_xkb(xkb: &str) -> Option<&'static str> {
    let xkb_lower = xkb.to_lowercase();
    LAYOUTS
        .iter()
        .find(|e| e.xkb == xkb_lower)
        .map(|e| e.language)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_code_from_xkb() {
        assert_eq!(short_code_from_xkb("swe"), Some("SE"));
        assert_eq!(short_code_from_xkb("se"), Some("SE"));
        assert_eq!(short_code_from_xkb("us"), Some("US"));
        assert_eq!(short_code_from_xkb("de"), Some("DE"));
        assert_eq!(short_code_from_xkb("latam"), Some("LA"));
        assert_eq!(short_code_from_xkb("ara"), Some("AR"));
        assert_eq!(short_code_from_xkb("xyz"), None);
    }

    #[test]
    fn test_short_code_from_xkb_case_insensitive() {
        assert_eq!(short_code_from_xkb("SWE"), Some("SE"));
        assert_eq!(short_code_from_xkb("Us"), Some("US"));
    }

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
    fn test_language_from_xkb() {
        assert_eq!(language_from_xkb("swe"), Some("Swedish"));
        assert_eq!(language_from_xkb("se"), Some("Swedish"));
        assert_eq!(language_from_xkb("us"), Some("English"));
        assert_eq!(language_from_xkb("de"), Some("German"));
        assert_eq!(language_from_xkb("xyz"), None);
    }

    #[test]
    fn test_all_entries_have_uppercase_codes() {
        for entry in LAYOUTS {
            assert_eq!(
                entry.code,
                entry.code.to_uppercase(),
                "code for xkb '{}' should be uppercase",
                entry.xkb
            );
        }
    }

    #[test]
    fn test_all_entries_have_lowercase_xkb() {
        for entry in LAYOUTS {
            assert_eq!(
                entry.xkb,
                entry.xkb.to_lowercase(),
                "xkb code '{}' should be lowercase",
                entry.xkb
            );
        }
    }

    #[test]
    fn test_all_entries_have_capitalized_language() {
        for entry in LAYOUTS {
            assert!(
                entry.language.starts_with(|c: char| c.is_uppercase()),
                "language '{}' for xkb '{}' should be capitalized",
                entry.language,
                entry.xkb
            );
        }
    }
}
