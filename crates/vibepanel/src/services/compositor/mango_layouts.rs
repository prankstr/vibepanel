//! Mango's built-in window-layout catalog.

/// Semantic metadata for one Mango layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MangoLayout {
    pub name: &'static str,
    pub symbol: &'static str,
    pub label: &'static str,
}

/// Layouts from Mango's `src/layout/layout.h`, in upstream order.
pub const MANGO_LAYOUTS: &[MangoLayout] = &[
    MangoLayout {
        name: "tile",
        symbol: "T",
        label: "Tile",
    },
    MangoLayout {
        name: "scroller",
        symbol: "S",
        label: "Scroller",
    },
    MangoLayout {
        name: "grid",
        symbol: "G",
        label: "Grid",
    },
    MangoLayout {
        name: "monocle",
        symbol: "M",
        label: "Monocle",
    },
    MangoLayout {
        name: "deck",
        symbol: "K",
        label: "Deck",
    },
    MangoLayout {
        name: "center_tile",
        symbol: "CT",
        label: "Center Tile",
    },
    MangoLayout {
        name: "right_tile",
        symbol: "RT",
        label: "Right Tile",
    },
    MangoLayout {
        name: "vertical_scroller",
        symbol: "VS",
        label: "Vertical Scroller",
    },
    MangoLayout {
        name: "vertical_tile",
        symbol: "VT",
        label: "Vertical Tile",
    },
    MangoLayout {
        name: "vertical_grid",
        symbol: "VG",
        label: "Vertical Grid",
    },
    MangoLayout {
        name: "vertical_deck",
        symbol: "VK",
        label: "Vertical Deck",
    },
    MangoLayout {
        name: "dwindle",
        symbol: "DW",
        label: "Dwindle",
    },
    MangoLayout {
        name: "fair",
        symbol: "F",
        label: "Fair",
    },
    MangoLayout {
        name: "vertical_fair",
        symbol: "VF",
        label: "Vertical Fair",
    },
];

pub fn by_name(name: &str) -> Option<&'static MangoLayout> {
    MANGO_LAYOUTS.iter().find(|layout| layout.name == name)
}

pub fn by_symbol(symbol: &str) -> Option<&'static MangoLayout> {
    MANGO_LAYOUTS.iter().find(|layout| layout.symbol == symbol)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn catalog_has_unique_names_and_symbols() {
        assert_eq!(MANGO_LAYOUTS.len(), 14);
        assert_eq!(
            MANGO_LAYOUTS
                .iter()
                .map(|layout| layout.name)
                .collect::<HashSet<_>>()
                .len(),
            MANGO_LAYOUTS.len()
        );
        assert_eq!(
            MANGO_LAYOUTS
                .iter()
                .map(|layout| layout.symbol)
                .collect::<HashSet<_>>()
                .len(),
            MANGO_LAYOUTS.len()
        );
    }
}
