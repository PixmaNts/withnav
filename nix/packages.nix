# The `nav` binary and the VS Code extension, built from this checkout.
{ pkgs, src }:
let
  inherit (pkgs) lib rustPlatform buildNpmPackage importNpmLock;
  version = (lib.importTOML "${src}/Cargo.toml").package.version;
in
{
  # Single binary `nav`; the language server is `nav lsp`. The WebUI is embedded from the
  # committed `webui/dist`, so no npm build is needed here.
  withnav = rustPlatform.buildRustPackage {
    pname = "withnav";
    inherit version src;
    cargoLock.lockFile = "${src}/Cargo.lock";
    # Several tests call model gateways over the network, which the Nix sandbox forbids.
    doCheck = false;
    meta = {
      description = "Pair-programming agent with a navigator for VS Code";
      mainProgram = "nav";
      platforms = lib.platforms.linux;
    };
  };

  # Laid out like the nixpkgs extensions (share/vscode/extensions/<id>) so Home Manager's
  # programs.vscode.profiles.<name>.extensions can link it.
  vscode-withnav = buildNpmPackage {
    pname = "vscode-withnav";
    inherit version;
    src = "${src}/vscode-withnav";
    npmDeps = importNpmLock { npmRoot = "${src}/vscode-withnav"; };
    npmConfigHook = importNpmLock.npmConfigHook;
    # keytar (pulled in by vsce, the publishing tool) compiles C against libsecret;
    # the extension itself needs no install script.
    npmFlags = [ "--ignore-scripts" ];
    npmBuildScript = "compile";
    installPhase = ''
      dir=$out/share/vscode/extensions/pixmants.withnav
      mkdir -p $dir
      cp -r package.json out media README.md $dir/
    '';
    passthru = {
      vscodeExtPublisher = "pixmants";
      vscodeExtName = "withnav";
      vscodeExtUniqueId = "pixmants.withnav";
    };
  };
}
