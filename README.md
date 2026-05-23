# VibePanel

<p align="center">
  <a href="https://github.com/prankstr/vibepanel/stargazers"><img src="https://img.shields.io/github/stars/prankstr/vibepanel?style=for-the-badge&labelColor=101418&color=adabe0" alt="GitHub stars"></a>
  <a href="https://github.com/prankstr/vibepanel/releases"><img src="https://img.shields.io/github/v/release/prankstr/vibepanel?style=for-the-badge&labelColor=101418&color=adabe0" alt="GitHub release"></a>
  <a href="https://aur.archlinux.org/packages/vibepanel-bin"><img src="https://img.shields.io/aur/version/vibepanel-bin?style=for-the-badge&labelColor=101418&color=adabe0" alt="AUR version"></a>
  <a href="https://github.com/prankstr/vibepanel/blob/main/LICENSE"><img src="https://img.shields.io/github/license/prankstr/vibepanel?style=for-the-badge&labelColor=101418&color=adabe0" alt="License"></a>
  <br>
  <img src="assets/screenshots/islands_bar_dark.png" alt="VibePanel" width="830">
</p>

A batteries-included Wayland panel that replaces your status bar, notification daemon, OSD and more with a single binary. Works out of the box with [Hyprland](https://github.com/hyprwm/Hyprland), [Niri](https://github.com/niri-wm/niri), [Sway](https://github.com/swaywm/sway), [MangoWC](https://github.com/mangowm/mango) and other compositors.

## Why VibePanel?

VibePanel is something between a simple status bar and a full desktop shell:

- **Fast & native** – Single Rust binary with GTK4. Direct system integration, low resource usage.
- **Batteries included** – VibePanel replaces several common components with a single binary:
  - **Notifications** – Integrated notification center
  - **OSD** – Built-in on-screen display for volume and brightness
  - **Quick settings** – Native panel for Wi‑Fi, Bluetooth, audio, power profiles and more
- **Minimal config** – Sensible defaults out of the box; customize with TOML, CSS only if needed.
- **Modern aesthetics** – Defaults to a floating "island" design with instant hot‑reloading and features wallpaper adaptive theming that auto‑switches between light and dark.
- **Integrated CLI** – Control volume, brightness, media playback, bar visibility, popovers and idle inhibition.

## Demo

These examples use roughly ~10–35 lines of TOML to get completely different vibes, no CSS required.

https://github.com/user-attachments/assets/fba27921-0886-4e7b-850d-b51341583693

*A few example configurations*
<table align="center">
  <tr>
    <td><a href="assets/screenshots/gruvbox_desktop.png"><img src="assets/screenshots/gruvbox_desktop.png" width="270"></a></td>
    <td><a href="assets/screenshots/frosted_minimal_desktop.png"><img src="assets/screenshots/frosted_minimal_desktop.png" width="270"></a></td>
    <td><a href="assets/screenshots/sonoma_desktop.png"><img src="assets/screenshots/sonoma_desktop.png" width="270"></a></td>
  </tr>
</table>

## Widgets

- **Quick settings**:
  - **Audio** - Control volume and outputs
  - **Brightness** - Adjust screen brightness
  - **Bluetooth** - Manage and pair devices
  - **Wi-Fi** - Connect to and manage networks
  - **VPN** - Connect to NetworkManager-managed VPN connections
  - **Idle Inhibitor** - Toggle idle inhibitor to prevent sleep
- **Workspaces** - clickable indicators with tooltips
- **Window title** - active window with app icon
- **Keyboard layout** - layout indicator with click to cycle
- **Clock** - configurable format with calendar popover
- **Battery** - status with detailed popover and power profiles
- **System tray** - XDG tray support
- **Notifications** - notification center with Do Not Disturb
- **Updates** - package update indicator (dnf, pacman/paru and flatpak support)
- **CPU, Memory, GPU & Network Speed** - system resource monitors (AMD and NVIDIA GPU support)
- **Media** - MPRIS media player controls with album art
- **Custom** - user-defined widgets (scripts, buttons, indicators)
- **Taskbar** - open windows as clickable buttons

## Quickstart

1. Install VibePanel:

   **Arch Linux (AUR):**

   ```sh
   yay -S vibepanel-bin
   ```

   **Fedora:**

    ```sh
    # Install RPM with the Terra Repository: https://terrapkg.com/
    sudo dnf install --nogpgcheck --repofrompath 'terra,https://repos.fyralabs.com/terra$releasever' terra-release
    sudo dnf install vibepanel
    ```

     ```sh
     # Install with COPR
     sudo dnf copr enable prankstr/vibepanel
     sudo dnf install vibepanel
     ```

   **Nix:**

   ```sh
   # Try it
   nix run github:prankstr/vibepanel

   # Install
   nix profile install github:prankstr/vibepanel
   ```

   **Other distros:** Install [runtime dependencies](https://github.com/prankstr/vibepanel/wiki/Installation#runtime-dependencies), then:

   ```sh
   curl -LO https://github.com/prankstr/vibepanel/releases/latest/download/vibepanel-x86_64-unknown-linux-gnu
   install -Dm755 vibepanel-x86_64-unknown-linux-gnu ~/.local/bin/vibepanel
   ```

   Or [build from source](https://github.com/prankstr/vibepanel/wiki/Installation#from-source).

2. Run it:

   ```sh
   vibepanel
   ```

See the [Installation wiki](https://github.com/prankstr/vibepanel/wiki/Installation) for more information.

## Configuration

VibePanel doesn't require a config file to run, but if you want to customize anything, create a config at `~/.config/vibepanel/config.toml`:

```sh
mkdir -p ~/.config/vibepanel
vibepanel --print-example-config > ~/.config/vibepanel/config.toml
```

Here's a minimal example:

```toml
[bar]
size = 32

[widgets]
left = ["workspaces", "window_title"]
center = ["media"]
right = ["quick_settings", "battery", "clock", "notifications"]

[theme]
mode = "dark"
accent = "#adabe0"
```

Changes hot-reload instantly. See the [Configuration wiki](https://github.com/prankstr/vibepanel/wiki/Configuration) for all options.

## Status

VibePanel is pre-1.0 and under active development.
Config options and defaults may change between minor releases, check the changelog when upgrading.

I built the first version in Python but wanted to migrate to Rust for performance, portability and simply to learn Rust.
The porting took waay too long in a language I was still learning so I've written the code with a lot of AI assistance.
I review all code and use VibePanel daily on multiple computers.

### Compatibility

- **Compositors:** [Hyprland](https://github.com/hyprwm/Hyprland), [Niri](https://github.com/niri-wm/niri), [Sway](https://github.com/swaywm/sway), [Miracle WM](https://github.com/miracle-wm-org/miracle-wm), [Scroll](https://github.com/dawsers/scroll) and other i3-IPC compatible compositors. [MangoWC](https://github.com/mangowm/mango)/[DWL](https://codeberg.org/dwl/dwl) via dwl-ipc.
- **Updates widget:** dnf, pacman/paru and flatpak.

## Documentation

Full documentation lives in the [wiki](https://github.com/prankstr/vibepanel/wiki):

- [Installation](https://github.com/prankstr/vibepanel/wiki/Installation) - Dependencies, building, auto-start
- [Configuration](https://github.com/prankstr/vibepanel/wiki/Configuration) - All config options
- [CLI](https://github.com/prankstr/vibepanel/wiki/CLI) - Command reference
- [Widgets](https://github.com/prankstr/vibepanel/wiki/Widgets) - Widget reference and per-widget options
- [Theming](https://github.com/prankstr/vibepanel/wiki/Theming) - Custom CSS styling
- [CSS Variables](https://github.com/prankstr/vibepanel/wiki/CSS-Variables) - Full CSS variable reference

## Contributing

- Found a bug? [Open an issue](https://github.com/prankstr/vibepanel/issues)
- Want a feature? [Request it](https://github.com/prankstr/vibepanel/issues)
- Pull requests welcome

If you find VibePanel useful, consider giving it a star. It helps others discover the project.

## License

MIT
