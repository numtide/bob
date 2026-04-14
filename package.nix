{
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "bob";
  version = (lib.importTOML ./Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  meta = {
    description = "Bob the Builder — fast incremental builds on top of Nix";
    homepage = "https://github.com/numtide/bob";
    license = lib.licenses.mit;
    mainProgram = "bob";
    platforms = lib.platforms.unix;
  };
}
