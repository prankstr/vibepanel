#!/usr/bin/env bash
# Layer-shell contracts in a private headless compositor, never the user's session.
set -euo pipefail
runtime=$(mktemp -d /tmp/vibepanel-auto-hide-test.XXXXXX)
chmod 700 "$runtime"
backend=${2:-sway}
case "$backend" in sway|mango) ;; *) echo "Backend must be sway or mango" >&2; exit 2 ;; esac
compositor_pid=
cleanup() {
    if [[ -n "$compositor_pid" ]]; then
        kill "$compositor_pid" 2>/dev/null || true
        wait "$compositor_pid" 2>/dev/null || true
    fi
    # Keep logs for diagnosis; sockets are removed by the compositor on exit.
    printf 'Headless compositor logs: %s/compositor.log\n' "$runtime"
}
trap cleanup EXIT
if [[ "$backend" == "sway" ]]; then
cat > "$runtime/config" <<'EOF'
xwayland disable
output HEADLESS-1 mode 1280x800
seat seat0 fallback true
seat seat0 hide_cursor 0
EOF
env -u WAYLAND_DISPLAY -u DISPLAY XDG_RUNTIME_DIR="$runtime" WLR_BACKENDS=headless WLR_RENDERER=pixman \
    sway --unsupported-gpu -c "$runtime/config" > "$runtime/compositor.log" 2>&1 &
else
    printf 'animations=0\nmonitorrule=name:HEADLESS-1,scale:1.8,width:2304,height:1440\n' > "$runtime/config"
    env -u WAYLAND_DISPLAY -u DISPLAY XDG_RUNTIME_DIR="$runtime" WLR_BACKENDS=headless WLR_RENDERER=pixman \
        mango -c "$runtime/config" > "$runtime/compositor.log" 2>&1 &
fi
compositor_pid=$!
for ((attempt=0; attempt<100; attempt++)); do
    wayland_sockets=("$runtime"/wayland-*)
    if [[ "$backend" == "sway" ]]; then
        ipc_sockets=("$runtime"/sway-ipc.*.sock)
    else
        ipc_sockets=("$runtime"/mango-*.sock)
    fi
    if [[ -S "${wayland_sockets[0]}" && -S "${ipc_sockets[0]}" ]]; then break; fi
    if ! kill -0 "$compositor_pid" 2>/dev/null; then cat "$runtime/compositor.log"; exit 1; fi
    sleep 0.05
done
[[ -S "${wayland_sockets[0]}" && -S "${ipc_sockets[0]}" ]]
if [[ "$backend" == "mango" ]]; then
    export MANGO_INSTANCE_SIGNATURE="${ipc_sockets[0]}"
    unset SWAYSOCK
    sway_test=0
else
    unset MANGO_INSTANCE_SIGNATURE
    export SWAYSOCK="${ipc_sockets[0]}"
    sway_test=1
fi
env -u NIRI_SOCKET -u HYPRLAND_INSTANCE_SIGNATURE -u MIRACLESOCK \
    XDG_RUNTIME_DIR="$runtime" WAYLAND_DISPLAY="${wayland_sockets[0]}" \
    GDK_BACKEND=wayland GSK_RENDERER="${GSK_RENDERER:-cairo}" VIBEPANEL_HEADLESS_SWAY_TEST="$sway_test" VIBEPANEL_UI_REGRESSION_REQUIRED=1 \
    dbus-run-session -- python3 scripts/headless-test-pointer.py \
    cargo test -p vibepanel "${1:-test_layer_shell_}" -- --ignored --test-threads=1
