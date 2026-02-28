#!/usr/bin/env bash
# Subsets MaterialSymbolsRounded to only the glyphs used in icons.rs.
#
# Requirements (for subsetting only, not --check):
#   Python 3 with fonttools: pip install fonttools
#
# The full font is ~14 MB with thousands of glyphs. We only use ~90 icons,
# so subsetting saves ~13.8 MB from the binary (the font is embedded via
# include_bytes!).
#
# Usage:
#   ./scripts/subset-font.sh          # download full font if needed, subset
#   ./scripts/subset-font.sh --check  # verify manifest matches icons.rs
#   PYTHON=python3.12 ./scripts/subset-font.sh  # custom python
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
ICONS_RS="$PROJECT_ROOT/crates/vibepanel/src/services/icons.rs"
FULL_FONT="$PROJECT_ROOT/assets/fonts/MaterialSymbolsRounded-full.ttf"
SUBSET_FONT="$PROJECT_ROOT/assets/fonts/MaterialSymbolsRounded.ttf"
MANIFEST="$PROJECT_ROOT/assets/fonts/glyphs.txt"

FONT_URL="https://raw.githubusercontent.com/google/material-design-icons/master/variablefont/MaterialSymbolsRounded%5BFILL%2CGRAD%2Copsz%2Cwght%5D.ttf"

PYTHON="${PYTHON:-python3}"

# Extract unique Material Symbol glyph names from the RHS of match arms
# in material_symbol_lookup() (the inner lookup table used by material_symbol_name).
extract_glyphs() {
    sed -n '/^fn material_symbol_lookup/,/^}/p' "$ICONS_RS" \
        | sed -n 's/.*=> "\([a-z_0-9]*\)".*/\1/p' \
        | sort -u
}

# --- Check mode: verify manifest matches icons.rs (no Python needed) ---
if [ "${1:-}" = "--check" ]; then
    if [ ! -f "$ICONS_RS" ]; then
        echo "ERROR: Cannot find $ICONS_RS" >&2
        exit 1
    fi
    if [ ! -f "$MANIFEST" ]; then
        echo "ERROR: $MANIFEST not found." >&2
        echo "Run: ./scripts/subset-font.sh" >&2
        exit 1
    fi

    current=$(extract_glyphs)
    committed=$(cat "$MANIFEST")

    if [ "$current" != "$committed" ]; then
        echo "Font glyph manifest is out of date!" >&2
        echo "" >&2
        diff <(echo "$committed") <(echo "$current") >&2 || true
        echo "" >&2
        echo "Run ./scripts/subset-font.sh to update the subset font and manifest." >&2
        exit 1
    fi

    echo "Font glyph manifest is up to date ($(echo "$committed" | wc -l) glyphs)."
    exit 0
fi

# --- Subset mode ---
if [ ! -f "$ICONS_RS" ]; then
    echo "ERROR: Cannot find $ICONS_RS" >&2
    exit 1
fi

# Download the full font if not present locally
if [ ! -f "$FULL_FONT" ]; then
    echo "Full font not found, downloading from Google Material Design Icons..."
    curl -fSL -o "$FULL_FONT" "$FONT_URL"
    echo "Downloaded to $FULL_FONT"
fi

# Check python + fonttools are available
if ! "$PYTHON" -c "from fontTools import subset" 2>/dev/null; then
    echo "ERROR: Python fonttools not found. Install with: pip install fonttools" >&2
    exit 1
fi

glyphs=$(extract_glyphs)
count=$(echo "$glyphs" | wc -l)
echo "Found $count unique Material Symbol glyphs"

# Write the manifest (one glyph name per line, sorted)
echo "$glyphs" > "$MANIFEST"

echo "Subsetting font..."

# Material Symbols uses GSUB ligature substitution: typing "wifi" triggers a
# ligature rule that maps to the wifi glyph. The standard pyftsubset --text
# approach doesn't work here because ALL icon ligatures share the same ASCII
# input characters (a-z, 0-9, _), so GSUB closure pulls in every icon.
#
# Instead we use the fontTools Python API with layout_closure=False to:
# 1. Keep only the exact icon glyphs we need (+ their .fill variants + ASCII
#    input characters for ligature matching)
# 2. Retain GSUB ligature rules whose inputs AND outputs are all in our set
# 3. Preserve variable font axes (wght, FILL, GRAD, opsz)
"$PYTHON" - "$FULL_FONT" "$SUBSET_FONT" "$MANIFEST" << 'PYEOF'
import sys
from fontTools.ttLib import TTFont
from fontTools import subset

font_path, output_path, manifest_path = sys.argv[1], sys.argv[2], sys.argv[3]

font = TTFont(font_path)
glyph_order = set(font.getGlyphOrder())

# Read desired glyph names from manifest
with open(manifest_path) as f:
    wanted = set(line.strip() for line in f if line.strip())

# Bail early if any glyph names are missing (catches typos in icons.rs)
missing = wanted - glyph_order
if missing:
    print(f"ERROR: glyphs not found in font: {sorted(missing)}", file=sys.stderr)
    sys.exit(1)

# Collect target glyphs: icon glyphs + .fill variants
target_glyphs = {'.notdef'}
for name in wanted:
    target_glyphs.add(name)
    fill = name + '.fill'
    if fill in glyph_order:
        target_glyphs.add(fill)

# Collect unicode codepoints for ASCII chars used in ligature input
input_unicodes = set()
for name in wanted:
    for ch in name:
        input_unicodes.add(ord(ch))

options = subset.Options()
options.layout_features = ['rlig', 'rclt', 'liga', 'clig', 'calt']
options.desubroutinize = True
options.hinting = False
options.notdef_outline = True
options.retain_gids = False
# Prevent GSUB closure from pulling in all ligature targets
options.layout_closure = False

subsetter = subset.Subsetter(options=options)
subsetter.populate(unicodes=input_unicodes, glyphs=target_glyphs)
subsetter.subset(font)
font.save(output_path)

# Verify
result = TTFont(output_path)
glyph_count = len(result.getGlyphOrder())

# Count retained ligatures
lig_count = 0
if 'GSUB' in result:
    gsub = result['GSUB'].table
    for lookup in gsub.LookupList.Lookup:
        for st in lookup.SubTable:
            if hasattr(st, 'ExtSubTable'):
                st = st.ExtSubTable
            if hasattr(st, 'ligatures'):
                lig_count += sum(len(ligs) for ligs in st.ligatures.values())

# Check axes
axes = []
if 'fvar' in result:
    axes = [a.axisTag for a in result['fvar'].axes]

print(f"  Glyphs: {glyph_count}, Ligatures: {lig_count}, Axes: {axes}")
PYEOF

full_size=$(stat --printf='%s' "$FULL_FONT" 2>/dev/null || stat -f '%z' "$FULL_FONT")
subset_size=$(stat --printf='%s' "$SUBSET_FONT" 2>/dev/null || stat -f '%z' "$SUBSET_FONT")
saved=$((full_size - subset_size))

echo ""
echo "Full font:   $(numfmt --to=iec-i --suffix=B "$full_size" 2>/dev/null || echo "${full_size} bytes")"
echo "Subset font: $(numfmt --to=iec-i --suffix=B "$subset_size" 2>/dev/null || echo "${subset_size} bytes")"
echo "Saved:       $(numfmt --to=iec-i --suffix=B "$saved" 2>/dev/null || echo "${saved} bytes")"
echo ""
echo "Wrote subset to: $SUBSET_FONT"
echo "Wrote manifest:  $MANIFEST"
