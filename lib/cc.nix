# Mark a stdenv derivation as a bob cc unit.
#
# `bobCcSrc` is the *only* contract between bob.nix and the cc backend:
#   - presence makes `is_unit` true (the drv is replayed, not realised),
#   - the value is the live source dir (relative to repo root) that bob hashes
#     for change detection and overrides `$src` to at build time.
#
# `__structuredAttrs` is forced on so the attr survives into `.attrs.json`
# (the executor's structured-attrs path is also the only one that rewrites
# `src` to the live dir generically).
#
# Usage in a repo's bob.nix:
#
#   let bobCc = import "${bob}/lib/cc.nix"; in
#   {
#     workspaceMembers = …;             # rust backend
#     cc = bobCc.units {
#       libnrt = { drv = neuron.libnrt; src = "extra-code/b16/aws-neuron-runtime"; };
#     };
#   }
#
# `bob build libnrt` then resolves to `cc.libnrt`.
let
  unit =
    src: drv:
    drv.overrideAttrs (old: {
      __structuredAttrs = true;
      bobCcSrc = src;
      # Replayed builds skip unpack/patch and point cmake/meson at the live
      # tree; in-sandbox `nix build` of the same drv must still work, so leave
      # the original phases intact — bob's hook sets dontUnpack/dontPatch at
      # replay time only.
    });
in
{
  inherit unit;

  # Convenience: `{ name = { drv, src }; … }` → `{ name = unit src drv; … }`.
  units = builtins.mapAttrs (_: v: unit v.src v.drv);
}
