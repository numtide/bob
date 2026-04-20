# Declare bob cc units without perturbing their drv hash.
#
# `unit` attaches `bobCcSrc` as a *Nix-level* attribute (`drv // { … }`), not
# via `overrideAttrs`, so `drvPath` is unchanged. That's the whole point: the
# drv referenced by other consumers (e.g. a Rust cdylib's `buildInputs`) and
# the one under `cc.<name>` are the same store path, so bob's graph walk
# from a Rust root finds it as a unit and the C-edit cascade reaches the
# `.so` without any overlay plumbing.
#
# bob's cc backend evaluates
#
#   builtins.mapAttrs (_: v: { drv = v.drvPath; src = v.bobCcSrc; })
#     ((import bob.nix {}).cc or {})
#
# once (cached on bob.nix content) and uses the resulting drvPath→src map
# for `is_unit` and source-change tracking. Nothing reaches the drv env.
#
# Usage in a repo's bob.nix:
#
#   let bobCc = import "${bob}/lib/cc.nix"; in
#   {
#     workspaceMembers = …;
#     cc = bobCc.units {
#       ndl = { drv = neuron.ndl; src = "extra-code/b16/aws-neuron-kmdlib"; };
#     };
#   }
let
  unit = src: drv: drv // { bobCcSrc = src; };
in
{
  inherit unit;
  units = builtins.mapAttrs (_: v: unit v.src v.drv);
}
