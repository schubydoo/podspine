{
  description = "Self-hosted server that turns audiobooks into per-chapter podcast feeds";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        podspine = pkgs.rustPlatform.buildRustPackage {
          pname = "podspine";
          version = "1.1.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          # Build only the server binary (the workspace also holds a POC CLI crate).
          cargoBuildFlags = [ "--bin" "podspine" ];

          # The integration tests shell out to ffmpeg and synthesize fixtures; skip
          # them in the sandboxed build (they run in CI instead).
          doCheck = false;

          # ffmpeg/ffprobe are runtime dependencies — put them on the binary's PATH.
          postInstall = ''
            wrapProgram $out/bin/podspine \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.ffmpeg ]}
          '';

          meta = with pkgs.lib; {
            description = "Self-hosted server that turns audiobooks into per-chapter podcast feeds";
            homepage = "https://github.com/schubydoo/podspine";
            license = licenses.agpl3Only;
            mainProgram = "podspine";
            platforms = platforms.unix;
          };
        };
      in
      {
        packages.default = podspine;
        packages.podspine = podspine;
        apps.default = flake-utils.lib.mkApp { drv = podspine; };
      }
    )
    // {
      # NixOS service module: services.podspine.enable = true;
      nixosModules.default = import ./nix/module.nix self;

      overlays.default = final: prev: {
        podspine = self.packages.${prev.stdenv.hostPlatform.system}.podspine;
      };
    };
}
