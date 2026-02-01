Name:           vibepanel
# x-release-please-start-version
Version:        0.7.0
# x-release-please-end
Release:        1%{?dist}
Summary:        A GTK4 panel for Wayland with notifications, OSD, and quick settings

License:        MIT
URL:            https://github.com/prankstr/vibepanel
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz

BuildRequires:  rust
BuildRequires:  cargo
BuildRequires:  gcc
BuildRequires:  gtk4-devel
BuildRequires:  gtk4-layer-shell-devel
BuildRequires:  pulseaudio-libs-devel
BuildRequires:  systemd-devel
BuildRequires:  dbus-devel

Requires:       gtk4
Requires:       gtk4-layer-shell
Requires:       pulseaudio-libs
Requires:       upower
Requires:       NetworkManager
Requires:       bluez

Recommends:     power-profiles-daemon

%description
A GTK4 panel for Wayland with integrated notifications, OSD, and quick settings.
Supports Hyprland, Niri, MangoWC and DWL.

%prep
%autosetup

%build
# Use vendored dependencies (offline build)
cargo build --release --offline

%install
install -Dm755 target/release/vibepanel %{buildroot}%{_bindir}/vibepanel

%files
%license LICENSE
%{_bindir}/vibepanel

%changelog
* Sat Jan 17 2026 prankstr
- See https://github.com/prankstr/vibepanel/releases for changelog
