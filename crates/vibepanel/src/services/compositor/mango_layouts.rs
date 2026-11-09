//! Mango's built-in window-layout catalog.

use tr::tr;

/// Semantic metadata for one Mango layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MangoLayout {
    pub name: &'static str,
    pub symbol: &'static str,
}

/// Layouts from Mango's `src/layout/layout.h`, in upstream order.
pub const MANGO_LAYOUTS: &[MangoLayout] = &[
    MangoLayout {
        name: "tile",
        symbol: "T",
    },
    MangoLayout {
        name: "scroller",
        symbol: "S",
    },
    MangoLayout {
        name: "grid",
        symbol: "G",
    },
    MangoLayout {
        name: "monocle",
        symbol: "M",
    },
    MangoLayout {
        name: "deck",
        symbol: "K",
    },
    MangoLayout {
        name: "center_tile",
        symbol: "CT",
    },
    MangoLayout {
        name: "right_tile",
        symbol: "RT",
    },
    MangoLayout {
        name: "vertical_scroller",
        symbol: "VS",
    },
    MangoLayout {
        name: "vertical_tile",
        symbol: "VT",
    },
    MangoLayout {
        name: "vertical_grid",
        symbol: "VG",
    },
    MangoLayout {
        name: "vertical_deck",
        symbol: "VK",
    },
    MangoLayout {
        name: "dwindle",
        symbol: "DW",
    },
    MangoLayout {
        name: "fair",
        symbol: "F",
    },
    MangoLayout {
        name: "vertical_fair",
        symbol: "VF",
    },
];

impl MangoLayout {
    /// Translated display name.
    pub fn label(&self) -> String {
        match self.name {
            "tile" => tr!("Tile"),
            "scroller" => tr!("Scroller"),
            "grid" => tr!("Grid"),
            "monocle" => tr!("Monocle"),
            "deck" => tr!("Deck"),
            "center_tile" => tr!("Center Tile"),
            "right_tile" => tr!("Right Tile"),
            "vertical_scroller" => tr!("Vertical Scroller"),
            "vertical_tile" => tr!("Vertical Tile"),
            "vertical_grid" => tr!("Vertical Grid"),
            "vertical_deck" => tr!("Vertical Deck"),
            "dwindle" => tr!("Dwindle"),
            "fair" => tr!("Fair"),
            "vertical_fair" => tr!("Vertical Fair"),
            other => other.to_string(),
        }
    }
}

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
