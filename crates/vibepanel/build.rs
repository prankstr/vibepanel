// Ensure gtk4-layer-shell is linked before libwayland-client.
//
// gtk4-layer-shell works by shimming libwayland-client symbols (it defines
// the same symbols and intercepts calls GTK makes to libwayland). For this
// to work, libgtk4-layer-shell.so must be loaded by the dynamic linker
// BEFORE libwayland-client.so.
//
// Enabling the `wayland_crate` feature on gdk4-wayland adds wayland-client
// as a direct dependency, which can cause the linker to place
// libwayland-client.so before libgtk4-layer-shell.so in the binary. This
// build script forces the correct order by emitting an early link directive.
//
// See: https://github.com/wmww/gtk4-layer-shell/blob/main/linking.md

fn main() {
    // Force gtk4-layer-shell to appear first in the link order.
    // The `+whole-archive` / `-whole-archive` trick isn't needed here —
    // we just need the library to be listed before libwayland-client in
    // the linker command line. Emitting it from the top-level crate's
    // build script ensures it comes first.
    println!("cargo:rustc-link-lib=dylib=gtk4-layer-shell");
}
