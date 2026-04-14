{
  pkgs ? import <nixpkgs> { },
}:

pkgs.mkShell {
  inputsFrom = [ (pkgs.callPackage ./package.nix { }) ];
  packages = with pkgs; [
    cargo
    rustc
    rustfmt
    clippy
    rust-analyzer
  ];
  RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
}
