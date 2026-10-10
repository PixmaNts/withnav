{
  description = "WithNav: the `nav` binary and its VS Code extension";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      # `overlays.default` adds `withnav` (the `nav` binary) and `vscode-withnav` to pkgs.
      overlays.default = final: _prev: import ./nix/packages.nix { pkgs = final; src = self; };

      packages = forAllSystems (
        pkgs:
        let
          withnav = import ./nix/packages.nix { inherit pkgs; src = self; };
        in
        withnav // { default = withnav.withnav; }
      );
    };
}
