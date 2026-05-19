# UI Regression Tests

Vibepanel has production-backed UI regression tests for the bar and widget tree.
They are intentionally small and focus on regressions that unit tests do not
catch well: spacing, inherited style tokens, grouping seams, and rendered
colors.

## Running

The regular test suite does not present GTK UI windows:

```sh
cargo test
```

Run window-presenting GTK UI regression tests explicitly under Xvfb. The command
unsets `WAYLAND_DISPLAY` and forces GTK onto Xvfb's X11 display so no windows
appear on your real desktop session:

```sh
scripts/run-ui-regression-tests.sh
```

The local pre-commit hook runs this script when `xvfb-run` is installed. If Xvfb
is unavailable locally, the hook skips the window-presenting UI regression suite
and still runs `cargo test --all`, clippy, and rustfmt checks. CI runs the UI
regression script with Xvfb installed.

Layer-shell contracts require a Wayland compositor with layer-shell support, so
they stay ignored in the default suite:

```sh
cargo test -p vibepanel layer_shell -- --ignored --test-threads=1
```

The CI UI regression job does not run these layer-shell/compositor-dependent
contracts. CI uses Xvfb, which provides an X11 display, not a Wayland
compositor. Run layer-shell contracts locally under a real Wayland session with
layer-shell support when changing compositor protocol behavior.

To pause probe windows while debugging locally:

```sh
VIBEPANEL_UI_REGRESSION_PROBE_HOLD_MS=2000 cargo test -p vibepanel sectioned_bar -- --test-threads=1
```

## Structure

The bar UI regression tests use subprocess wrappers. GTK, display-level CSS
providers, and global config state are process-scoped, so each wrapper launches
one isolated contract via an internal ignored runner.

Keep window-presenting GTK UI regression tests ignored in the default suite,
name their runnable wrappers with the `test_ui_regression_` prefix, and run them
through the Xvfb script above. Keep internal runners, manual probes, and
compositor-dependent layer-shell contracts ignored as well.

## Pixel Sampling

Prefer sampling stable painted regions over exact visual edges. For background
checks, clear or minimize labels so center samples do not hit glyph pixels. Avoid
sampling borders, rounded corners, shadows, or antialiased edges unless that is
the behavior under test. When exact coordinates are fragile, scan a small region
for the expected color instead of asserting a single pixel.

## Test Tiers

The suite follows four tiers that map to the theming model:

| Tier | Purpose | Examples |
| --- | --- | --- |
| Core unit | Validate config parsing, theme derivation, and emitted CSS tokens without GTK. | `ThemePalette::css_vars_block`, outline/color/polarity tokens |
| CSS binding | Validate production selectors consume the expected CSS variables/classes. | `.widget`, `.vp-surface-popover`, notification rows |
| GTK UI regression | Validate Xvfb-compatible rendered layout and pixels. | bar spacing, widget pixels, grouping seams |
| Layer-shell | Validate compositor protocol state. Ignored by default and not run in CI's Xvfb job. | anchors, namespace, margins, exclusive zone |

CSS variable coverage should start with cheap regression tests: theme variables
consumed by production CSS should be emitted by the theme palette or explicitly
marked as optional hooks with safe fallbacks. Use GTK UI regression tests only
when a string-level assertion cannot prove the user-visible result.

Good UI regression candidates validate non-trivial propagation or rendering
paths: CSS variables composed with opacity/color-mix, scoped precedence such as
TOML versus user CSS, conditional values such as enabled/disabled outlines,
GTK/GSK-sensitive properties such as border alpha/radius, and historically
brittle areas like group seams. Do not add rendered tests mechanically for every
typography, icon, shadow, foreground, accent, or outgoing variable.

Layer-shell contracts should assert layer-shell behavior. If a test only checks
structure, CSS classes, or rendered colors, prefer a normal GTK UI regression
test so it can run in CI without a compositor.

## Theming Coverage

Use the theming docs as the coverage checklist. Prefer one high-signal contract
per behavior over mechanically testing every token in every tier.

| Theming area | Core unit | CSS contract | GTK UI regression | Layer-shell | Intentionally not covered / next gap |
| --- | --- | --- | --- | --- | --- |
| `theme.mode` dark/light | palette polarity tokens | shared surface selectors | dark/light widget pixel delta | n/a | `theme.mode = auto` / wallpaper-derived polarity |
| `theme.popover` polarity | popover token block | `.vp-surface-popover` vars | popover-vs-bar pixel delta | popover anchors/margins separately | n/a |
| `theme.states.urgent` | state tokens | urgent selectors | urgent workspace/taskbar pixels | n/a | Add `success` only if a real state-surface regression appears |
| `theme.states.warning` / critical notifications | critical background tokens | notification row selectors | n/a | toast window protocol separately | Revisit floating-toast critical styling in a follow-up PR |
| Hover state | hover tokens | hover vars consumed inside `:hover` selectors | intentionally not covered | n/a | `StateFlags::PRELIGHT` did not reliably activate GTK `:hover` under Xvfb; add rendered hover only when a stable trigger exists |
| Outline color/opacity | outline tokens and per-widget overrides | bar/widget/surface border vars | outline border pixels and CSS/GSK parity | n/a | n/a |
| `[bar]` size/spacing/inset/padding | token derivation | bar selector bindings | live bar measurements | bar anchor/exclusive-zone contracts | n/a |
| `[widgets]` background/radius/opacity | token derivation | `.widget`/popover bindings | widget/background pixels and grouping seams | n/a | radius/opacity pixels only if regressions recur |
| `[widgets.<name>]` overrides | per-widget CSS generation | scoped widget/popover selectors | override precedence pixels | n/a | Extend with a matrix only before adding more bespoke precedence pixels |
| User CSS overrides | n/a | selector reachability | override precedence pixels and runtime `style.css` load path | n/a | n/a |
| Surface families | surface token derivation | shared surface classes | popover pixels | popover/toast protocol separately | OSD, tray/menu, quick settings, toast pixels |

Known follow-ups:

- Add targeted coverage for `theme.mode = auto` / wallpaper-derived polarity.
- Add pango-font-rendering assertions by inspecting GTK label attributes rather
  than pixels.
- Add more surface-specific tests for OSD, tray/menu surfaces, and quick
  settings once shared UI regression helpers are reused outside `sectioned_bar.rs`.
