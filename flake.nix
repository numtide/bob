{
  description = "Bob the Builder — fast incremental builds on top of Nix";

  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.xz";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        bob = pkgs.rustPlatform.buildRustPackage {
          pname = "bob";
          version = (nixpkgs.lib.importTOML ./Cargo.toml).package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          meta = {
            description = "Fast incremental builds on top of Nix";
            homepage = "https://github.com/numtide/bob";
            license = nixpkgs.lib.licenses.mit;
            mainProgram = "bob";
          };
        };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.bob;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
          ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      checks = forAllSystems (pkgs: {
        bob = self.packages.${pkgs.stdenv.hostPlatform.system}.bob;
        fmt =
          pkgs.runCommand "cargo-fmt-check"
            {
              nativeBuildInputs = [
                pkgs.cargo
                pkgs.rustfmt
              ];
            }
            ''
              cd ${./.}
              cargo fmt --check
              touch $out
            '';
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
