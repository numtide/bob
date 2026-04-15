{
  description = "Bob the Builder — fast incremental builds on top of Nix";

  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.xz";

    devshell = {
      url = "github:numtide/devshell";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    cargo-nix-plugin = {
      url = "github:Mic92/cargo-nix-plugin";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      devshell,
      cargo-nix-plugin,
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

      # nix-instantiate with `builtins.resolveCargoWorkspace` available, for
      # `BOB_NIX_INSTANTIATE`. The plugin ABI is tied to the exact Nix it was
      # built against, so pair the 2.34 plugin with 2.34 nix from the same
      # nixpkgs rather than whatever the host has. Only exposed where
      # cargo-nix-plugin actually ships a build (no x86_64-darwin).
      bobNixInstantiate =
        pkgs:
        let
          system = pkgs.stdenv.hostPlatform.system;
        in
        nixpkgs.lib.optionalAttrs (cargo-nix-plugin.packages ? ${system}) {
          bob-nix-instantiate = pkgs.writeShellScriptBin "nix-instantiate" ''
            exec ${pkgs.nixVersions.nix_2_34}/bin/nix-instantiate \
              --option plugin-files ${
                cargo-nix-plugin.packages.${system}.cargo-nix-plugin-nix_2_34
              }/lib/nix/plugins \
              "$@"
          '';
        };
    in
    {
      packages = forAllSystems (
        pkgs:
        {
          inherit (import ./default.nix { inherit pkgs; }) bob default;
        }
        // bobNixInstantiate pkgs
      );

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
          ]
          ++ nixpkgs.lib.mapAttrsToList (_: drv: {
            name = "BOB_NIX_INSTANTIATE";
            value = "${drv}/bin/nix-instantiate";
          }) (bobNixInstantiate pkgs);
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
