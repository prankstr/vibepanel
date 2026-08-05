{
  description = "A GTK4 panel for Wayland with integrated notifications, OSD, and quick settings";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    crane.url = "github:ipetkov/crane";

    flake-utils.url = "github:numtide/flake-utils";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default;

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Common build inputs needed for both the deps-only and full builds
        nativeBuildInputs = with pkgs; [
          pkg-config
          wrapGAppsHook4
        ];

        buildInputs = with pkgs; [
          gtk4
          gtk4-layer-shell
          libpulseaudio
          udev
          dbus
          wayland
        ];

        # Filter source to only include Rust-relevant files (improves caching)
        # Also include .xml (Wayland protocol) and .ttf (embedded font) files
        src = pkgs.lib.cleanSourceWith {
          src = craneLib.path ./.;
          filter =
            path: type:
            (craneLib.filterCargoSources path type)
            || (pkgs.lib.hasSuffix ".xml" path)
            || (pkgs.lib.hasSuffix ".ttf" path);
        };

        commonArgs = {
          pname = "vibepanel";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
          inherit src nativeBuildInputs buildInputs;
          strictDeps = true;
        };

        # Build only the cargo dependencies so they can be cached
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Build the full package
        vibepanel = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;

            # Make runtime graphics drivers available to GTK.
            preFixup = ''
              gappsWrapperArgs+=(
                --suffix LD_LIBRARY_PATH : "${pkgs.addDriverRunpath.driverLink}/lib"
              )
            '';

            meta = {
              description = "A GTK4 panel for Wayland with integrated notifications, OSD, and quick settings";
              homepage = "https://github.com/prankstr/vibepanel";
              license = pkgs.lib.licenses.mit;
              mainProgram = "vibepanel";
              platforms = pkgs.lib.platforms.linux;
            };
          }
        );
      in
      {
        packages = {
          default = vibepanel;
          vibepanel = vibepanel;
        };

        devShells.default = craneLib.devShell {
          packages = with pkgs; [
            rust-analyzer
          ];

          # Make the build inputs available in the dev shell
          inputsFrom = [ vibepanel ];
        };
      }
    )
    // {
      overlays.default = final: prev: {
        vibepanel = self.packages.${final.system}.default;
      };
    };
}
