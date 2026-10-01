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
              ]);
            buildInputs = runtimeLibs;
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
          };
        }
      );

      overlays.default = final: _prev: {
        quillway = self.packages.${final.stdenv.hostPlatform.system}.default;
      };

      homeManagerModules.default = import ./nix/hm-module.nix self;
    };
}
