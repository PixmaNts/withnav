# Install

WithNav has two parts: the `nav` binary and the VS Code extension. Most people want the
extension, which can bring its own `nav`.

## Requirements

- Linux, x64 or arm64.
- VS Code 1.91 or later, if you use the extension.
- Native Windows is not supported. Use WSL or a VS Code Remote window and install the Linux
  package there.
- The extension is not on the VS Code Marketplace yet. Install it from a file.

## VS Code extension

Download a package from the
[GitHub Releases page](https://github.com/PixmaNts/withnav/releases) and install it:

```sh
code --install-extension withnav-linux-x64-<version>.vsix
```

Pick the package that matches your machine:

| File | Use it when | `nav` |
|---|---|---|
| `withnav-linux-x64-<version>.vsix` | Linux on x64 | Bundled (static binary). Nothing else to install. |
| `withnav-linux-arm64-<version>.vsix` | Linux on arm64 | Bundled (static binary). Nothing else to install. |
| `withnav-<version>.vsix` (universal) | macOS, Nix, development | Not included. Put `nav` on your `PATH`, or set `withnav.serverPath`. |

A path you set in `withnav.serverPath` always wins over the bundled binary. Set it to the
absolute path of `nav`. Do not add `lsp` to it; the extension adds that itself.

To check which binary the extension runs, open the **WithNav** output channel. Its first line is
`Using nav binary (...)`: setting, bundled or path.

## `nav` without the extension

Each release also publishes bare `nav` binaries as tarballs, and a `SHA256SUMS` file for all the
release files. Unpack a tarball and put `nav` on your `PATH`, for example in `~/.local/bin/`.

## Nix

The flake at the repository root builds both parts:

```sh
nix build .#withnav .#vscode-withnav
```

In a system flake, add this repository as an input and apply `overlays.default`. It provides:

- `pkgs.withnav`: the `nav` command.
- `pkgs.vscode-withnav`: the extension, for Home Manager's
  `programs.vscode.profiles.<name>.extensions`.

## From source

You need Rust 1.89 or later, and Node.js 18 or later for the extension.

```sh
cargo build --release --bins
install -m755 target/release/nav ~/.local/bin/
(cd vscode-withnav && npm ci && npm run package)
code --install-extension vscode-withnav/withnav-*.vsix
```

`npm run package` makes a universal package. It needs `nav` on your `PATH` or
`withnav.serverPath`. To build the release packages with a bundled binary, see
[the extension page](vscode.md#build-and-package).

## Check it works

```sh
nav --version
```

Then follow the [Quick start](quick-start.md).
