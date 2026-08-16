{
  description = "vdiff - visual PR review node graph";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        # Runtime libs the wgpu/winit GUI needs on Linux (Vulkan + X11/Wayland
        # windowing); Darwin uses Metal via system frameworks and needs none
        # of this. Standard nixpkgs wgpu-app pattern.
        linuxRuntimeLibs = with pkgs; [
          vulkan-loader
          wayland
          libxkbcommon
          xorg.libX11
          xorg.libXcursor
          xorg.libXrandr
          xorg.libXi
          libGL
        ];
      in {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "vdiff";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # The pipeline's fixture tests git-init real repos and shell out to
          # `git`, so the check phase needs it on PATH.
          nativeCheckInputs = [ pkgs.git ];

          nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [
            pkgs.makeWrapper
          ];
          buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux linuxRuntimeLibs;

          postFixup = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
            wrapProgram $out/bin/vdiff \
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath linuxRuntimeLibs}
          '';
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rust-analyzer
            pkgs.clippy
            pkgs.rustfmt

            # Pipeline integration tests git-init real fixture repos.
            pkgs.git
          ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux linuxRuntimeLibs;

          shellHook = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath linuxRuntimeLibs}:$LD_LIBRARY_PATH"
          '';
        };
      }
    );
}
