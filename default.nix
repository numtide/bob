{
  pkgs ? import <nixpkgs> { },
}:
let
  bob = pkgs.callPackage ./package.nix { };
in
{
  inherit bob;
  default = bob;

  devShell = import ./shell.nix { inherit pkgs; };
}
