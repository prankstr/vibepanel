# VibePanel

<p align="center">
  <img src="assets/screenshots/islands_bar_light.png" alt="VibePanel bar preview" width="830">
</p>

A GTK4 panel for Wayland with integrated notifications, OSD, and quick settings. Supports Hyprland, Niri, MangoWC and DWL.

## Why VibePanel?

VibePanel is something between a simple status bar and a full desktop shell:

- **Minimal config** – Sensible defaults out of the box; customize with TOML, CSS only if needed.
- **Batteries included** – VibePanel replaces several common components with a single binary:
  - **Notifications** – Integrated notification center
  - **OSD** – Built-in on-screen display for volume and brightness
  - **Quick settings** – Native panel for Wi‑Fi, Bluetooth, audio, power profiles and more
- **Modern aesthetics** – Defaults to a floating "island" style with instant hot‑reloading for layouts and themes.
- **Integrated CLI** – Small CLI for controlling volume, brightness, media controls and idle inhibition.
- **Center anchoring** – Custom GTK4 layout keeps center widgets centered even when left/right sections grow.

## Demo

These examples use roughly ~10–35 lines of TOML to get completely different vibes, no CSS required.

<https://github.com/user-attachments/assets/28cc8b6e-a1ec-46a9-acee-15c13ee2bce5>

*A few example configurations*
<table align="center">
  <tr>
    <td><a href="assets/screenshots/gruvbox_desktop.png"><img src="assets/screenshots/gruvbox_desktop.png" width="270"></a></td>
    <td><a href="assets/screenshots/frosted_minimal_desktop.png"><img src="assets/screenshots/frosted_minimal_desktop.png" width="270"></a></td>
    <td><a href="assets/screenshots/sonoma_desktop.png"><img src="assets/screenshots/sonoma_desktop.png" width="270"></a></td>
  </tr>
</table>

## Widgets

- **Workspaces** - clickable indicators with tooltips
- **Window title** - active window with app icon
- **Clock** - configurable format with calendar popover
- **Battery** - status with detailed popover and power profiles
- **System tray** - XDG tray support
- **Notifications** - notification center with Do Not Disturb
- **Updates** - package update indicator (dnf, pacman/paru, and flatpak support)
- **CPU & Memory** - system resource monitors
- **Media** - MPRIS media player controls with album art
- **Quick settings**:
  - **Audio** - Control volume and outputs
  - **Brightness** - Adjust screen brightness
  - **Bluetooth** - Manage and pair devices
  - **Wi-Fi** - Connect to and manage networks
  - **VPN** - Connect to NetworkManager-managed VPN connections
  - **Idle Inhibitor** - Toggle idle inhibitor to prevent sleep

## Quickstart

1. Install VibePanel:

   **Arch Linux (AUR):**

   ```sh
   yay -S vibepanel-bin
   ```

   **Fedora (COPR):**

   ```sh
   sudo dnf copr enable prankstr/vibepanel
   sudo dnf install vibepanel
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
touch ~/.config/vibepanel/config.toml
# or generate an example config
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

VibePanel is in early 0.x development but should be stable enough for daily use.
Config options and defaults may change between minor releases, check the changelog when upgrading.

The idea and architecture behind the panel is something I first built in Python.
For performance, portability and curiosity reasons I wanted to migrate to Rust. I started
doing it myself but realized it would take far too long in a language I'm still learning.
So this codebase is largely written by AI but a lot of effort has gone into making sure
it's not slop and I use VibePanel daily on multiple computers.

If you find a bug or if you're missing a feature, please [open an issue](https://github.com/prankstr/vibepanel/issues)!

### Compatibility

- **Compositors:** Hyprland, Niri, MangoWC/DWL. Sway support may be added based on demand.
- **Updates widget:** dnf, pacman/paru, and flatpak.

## Documentation

Full documentation lives in the [wiki](https://github.com/prankstr/vibepanel/wiki):

- [Installation](https://github.com/prankstr/vibepanel/wiki/Installation) - Dependencies, building, auto-start
- [Configuration](https://github.com/prankstr/vibepanel/wiki/Configuration) - All config options
- [Widgets](https://github.com/prankstr/vibepanel/wiki/Widgets) - Widget reference and per-widget options
- [Theming](https://github.com/prankstr/vibepanel/wiki/Theming) - Custom CSS styling
- [CSS Variables](https://github.com/prankstr/vibepanel/wiki/CSS-Variables) - Full CSS variable reference

## Contributing

Contributions are welcome! Feel free to open issues or submit pull requests.

## License

MIT
