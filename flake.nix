{
  description = "Incremental, read-only GitHub and GitLab metadata mirror";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "forge-sync";
            version = "0.1.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            FORGE_SYNC_GLAB_PATH = "${pkgs.glab}/bin/glab";
            # The build sandbox has no system trust store; reqwest's rustls
            # backend uses the platform verifier, so tests that build a
            # client need CA roots. Check phase only; not used at runtime.
            preCheck = ''
              export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
            '';
          };
        });

      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.rustfmt
              pkgs.clippy
              pkgs.pkg-config
              pkgs.glab
            ];
            FORGE_SYNC_GLAB_PATH = "${pkgs.glab}/bin/glab";
          };
        });

      checks = forAllSystems (system: {
        package = self.packages.${system}.default;
      });
    };
}
