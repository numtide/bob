# Entry point for `bob build <member>` in this repo. bob runs
# `nix-instantiate -E '(import <root>/bob.nix {}).rust.workspaceMembers.<m>.build'`,
# so this must work standalone (no flake context). Pins for nixpkgs and
# cargo-nix-plugin are read from flake.lock so there's a single source of
# truth.
#
# Requires `builtins.resolveCargoWorkspace`, i.e. a nix-instantiate with the
# cargo-nix-plugin loaded. The flake exposes one as
# `packages.<system>.bob-nix-instantiate`; point `BOB_NIX_INSTANTIATE` at it.
let
  lock = builtins.fromJSON (builtins.readFile ./flake.lock);

  fetchInput =
    name:
    let
      n = lock.nodes.${name}.locked;
    in
    builtins.fetchTarball {
      url = n.url or "https://github.com/${n.owner}/${n.repo}/archive/${n.rev}.tar.gz";
      sha256 = n.narHash;
    };
in
{
  pkgs ? import (fetchInput "nixpkgs") { },
  cargo-nix-plugin ? fetchInput "cargo-nix-plugin",
}:
let
  cargoNix = import "${cargo-nix-plugin}/lib" {
    inherit pkgs;
    src = ./.;
  };
in
{
  rust = { inherit (cargoNix) workspaceMembers allWorkspaceMembers; };
}
