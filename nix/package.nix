# Quillway, built with crane. `llama-cpp` provides `llama-server` on the
# wrapper's PATH; override it to pick a GPU backend, e.g.
#   quillway.override { llama-cpp = llama-cpp.override { cudaSupport = true; }; }
{
  lib,
  craneLib,
  pkg-config,
  makeWrapper,
  wayland,
  libxkbcommon,
  vulkan-loader,
  libGL,
  llama-cpp-vulkan,
  llama-cpp ? llama-cpp-vulkan,
}:
let
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../rustfmt.toml
    ];
  };
  common = {
    inherit src;
    strictDeps = true;
    nativeBuildInputs = [ pkg-config ];
    buildInputs = [ wayland libxkbcommon ];
  };
  cargoArtifacts = craneLib.buildDepsOnly (common // { pname = "quillway-deps"; });
  # wgpu/winit dlopen these at runtime; nothing links them at build time.
  runtimeLibs = [ wayland libxkbcommon vulkan-loader libGL ];
in
craneLib.buildPackage (
  common
  // {
    pname = "quillway";
    inherit cargoArtifacts;
    cargoExtraArgs = "-p quillway";
    nativeBuildInputs = common.nativeBuildInputs ++ [ makeWrapper ];
    # Workspace tests run in the flake's `checks`.
    doCheck = false;
    postFixup = ''
      patchelf --add-rpath ${lib.makeLibraryPath runtimeLibs} $out/bin/quillway
      wrapProgram $out/bin/quillway --prefix PATH : ${lib.makeBinPath [ llama-cpp ]}
    '';
    passthru = { inherit cargoArtifacts common; };
    meta = {
      description = "Shortcut-summoned local-LLM rewrite popup for Wayland (layer-shell)";
      license = with lib.licenses; [ mit asl20 ];
      mainProgram = "quillway";
      platforms = lib.platforms.linux;
    };
  }
)
