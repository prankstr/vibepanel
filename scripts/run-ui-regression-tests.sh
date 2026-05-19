#!/usr/bin/env sh
set -eu

if ! command -v xvfb-run >/dev/null 2>&1; then
    printf '%s\n' "error: xvfb-run is required for GTK UI regression tests"
    printf '%s\n' "install Xvfb so UI regression tests can run without presenting desktop windows"
    exit 1
fi

# Run only the ignored GTK UI regression wrappers. Internal runners are invoked
# by those wrappers and layer-shell contracts still need a real Wayland compositor.
echo '+xvfb-run -a env -u WAYLAND_DISPLAY GDK_BACKEND=x11 VIBEPANEL_UI_REGRESSION_REQUIRED=1 cargo test -p vibepanel test_ui_regression_ -- --ignored --test-threads=1'
xvfb-run -a env -u WAYLAND_DISPLAY GDK_BACKEND=x11 VIBEPANEL_UI_REGRESSION_REQUIRED=1 cargo test -p vibepanel test_ui_regression_ -- --ignored --test-threads=1
