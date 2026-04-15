{
  description = "Bob the Builder — fast incremental builds on top of Nix";

  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.xz";

    devshell = {
      url = "github:numtide/devshell";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      devshell,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system: f (nixpkgs.legacyPackages.${system}.extend devshell.overlays.default)
        );
    in
    {
      packages = forAllSystems (pkgs: {
        inherit (import ./default.nix { inherit pkgs; }) bob default;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.devshell.mkShell {
          # devshell installs bin/<name> -> entrypoint, which would shadow
          # the actual `bob` binary on PATH. Use a distinct name.
          name = "bob-dev";
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
          ];
          env = [
            {
              name = "RUST_SRC_PATH";
              value = "${pkgs.rustPlatform.rustLibSrc}";
            }
          ];
          commands = [
            {
              name = "fmt";
              help = "format nix + rust";
              command = ''
                ${pkgs.lib.getExe pkgs.nixfmt-tree} "$PRJ_ROOT"
                cargo fmt --manifest-path "$PRJ_ROOT/Cargo.toml"
              '';
            }
            {
              name = "lint";
              help = "cargo clippy";
              command = ''cargo clippy --manifest-path "$PRJ_ROOT/Cargo.toml" -- -D warnings'';
            }
          ];
        };
      });

      checks = forAllSystems (pkgs: {
        inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) bob;
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
        # bob-core must stay language-agnostic. Fail if any Rust-backend
        # identifier appears outside of doc comments.
        core-leakage = pkgs.runCommand "bob-core-leakage" { nativeBuildInputs = [ pkgs.ripgrep ]; } ''
          cd ${./crates/core/src}
          if rg --no-heading --line-number --pcre2 \
               '^(?!\s*//).*\b(crateName|crateType|crateLinks|crateVersion|libName|rustc|rlib|EXTRA_RUSTC_FLAGS|__rustc-wrap|buildRustCrate|Cargo\.(toml|lock))\b' \
               . ; then
            echo "error: Rust-backend identifiers found in bob-core (see above)" >&2
            exit 1
          fi
          touch $out
        '';
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
