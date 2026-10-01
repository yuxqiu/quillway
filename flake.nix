{
  description = "Quillway: a shortcut-summoned local-LLM rewrite popup for Wayland";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
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
      rust-overlay,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          let
            pkgs = import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            };
            toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
            craneLib = (crane.mkLib pkgs).overrideToolchain (_: toolchain);
          in
          f { inherit pkgs toolchain craneLib; }
        );
    in
    {
      packages = forAllSystems (
        { pkgs, craneLib, ... }:
        let
          quillway = pkgs.callPackage ./nix/package.nix { inherit craneLib; };
        in
        {
          default = quillway; # Vulkan llama.cpp: AMD, Intel and NVIDIA GPUs
          quillway = quillway;
          quillway-cpu = quillway.override { llamaCpp = pkgs.llama-cpp; };
        }
        # nixpkgs builds ROCm for x86_64 only.
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isx86_64 {
          quillway-rocm = quillway.override { llamaCpp = pkgs.llama-cpp-rocm; };
        }
      );

      checks = forAllSystems (
        { pkgs, craneLib, ... }:
        let
          pkg = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          inherit (pkg.passthru) cargoArtifacts common;
        in
        {
          build = pkg;
          clippy = craneLib.cargoClippy (
            common
            // {
              inherit cargoArtifacts;
              pname = "quillway-clippy";
              cargoClippyExtraArgs = "--workspace --all-targets -- --deny warnings";
            }
          );
          test = craneLib.cargoTest (
            common
            // {
              inherit cargoArtifacts;
              pname = "quillway-test";
              cargoTestExtraArgs = "--workspace";
            }
          );
          fmt = craneLib.cargoFmt { inherit (common) src; pname = "quillway-fmt"; };
        }
      );

      devShells = forAllSystems (
        { pkgs, toolchain, ... }:
        let
          runtimeLibs = with pkgs; [
            wayland
            libxkbcommon
            vulkan-loader
            libGL
          ];
        in
        {
          default = pkgs.mkShell {
            packages =
              [ toolchain ]
              ++ (with pkgs; [
                pkg-config
                cargo-nextest
                wayland-utils
                wev
                wl-clipboard
                llama-cpp-vulkan
                # scripts/screenshots.sh
                sway
                swaybg
                grim
                wtype
                imagemagick
                pngquant
                oxipng
              ]);
            buildInputs = runtimeLibs;
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
            # GPU drivers matching this nixpkgs, for scripts/screenshots.sh.
            QUILLWAY_SCREENSHOT_MESA = "${pkgs.mesa}";
            # Load Mesa's GPU drivers from this nixpkgs, not the host's: a host driver
            # built against a newer glibc fails to load here (e.g. AMD's, via LLVM), and
            # Quillway silently falls back to software rendering and llama-server to the
            # CPU. NVIDIA's proprietary driver isn't part of Mesa, so keep the host's.
            # Lavapipe (software Vulkan) is left out so nothing picks it over the GPU.
            shellHook = ''
              if [ ! -e /proc/driver/nvidia ]; then
                export GBM_BACKENDS_PATH=${pkgs.mesa}/lib/gbm
                export LIBGL_DRIVERS_PATH=${pkgs.mesa}/lib/dri
                export __EGL_VENDOR_LIBRARY_FILENAMES=${pkgs.mesa}/share/glvnd/egl_vendor.d/50_mesa.json
                VK_DRIVER_FILES=
                for icd in ${pkgs.mesa}/share/vulkan/icd.d/*.json; do
                  case $icd in */lvp_icd.*) ;; *) VK_DRIVER_FILES=$VK_DRIVER_FILES$icd: ;; esac
                done
                export VK_DRIVER_FILES
                unset icd
              fi
            '';
          };
        }
      );

      overlays.default = final: _prev: {
        quillway = self.packages.${final.stdenv.hostPlatform.system}.default;
      };

      homeManagerModules.default = import ./nix/hm-module.nix self;
    };
}
