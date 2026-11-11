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
    // xkb names every UK variant "English (UK, ...)", so English without a
    // country is nearly always a US variant.
    LayoutEntry {
        code: "US",
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
        let cases = [
            ("Swedish", Some("SE")),
            ("German", Some("DE")),
            ("Japanese", Some("JP")),
            ("Klingon", None),
            ("swedish", Some("SE")),
            ("GERMAN", Some("DE")),
        ];
        for (language, expected) in cases {
            assert_eq!(short_code_from_language(language), expected, "{language}");
        }

        for entry in LAYOUTS {
            assert_eq!(
                entry.code,
                entry.code.to_uppercase(),
                "layout code for '{}' should be uppercase",
                entry.language
            );
        }
    }
}
