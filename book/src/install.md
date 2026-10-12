# Install

WithNav has two parts: the `nav` binary and the VS Code extension. Most people want the
extension, which can bring its own `nav`.

## Requirements

- Linux, x64 or arm64; native Windows x64 packages are being prepared (see below).
- VS Code 1.91 or later, if you use the extension.
- Until a release includes the Windows assets below, use WSL or a VS Code Remote window with
  the Linux package. Windows packaging alone does not establish runtime support.
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
| `withnav-win32-x64-<version>.vsix` | Windows x64 compile/packaging preview | Bundled `nav.exe`; sessions are disabled. |
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

## Native Windows x64

The release pipeline builds `x86_64-pc-windows-msvc`, without a WSL or MSYS runtime dependency
for `nav.exe` itself. The CLI archive is `nav-<version>-x86_64-pc-windows-msvc.zip`, containing
`nav.exe` and the MIT/Apache license files. Extract it into a user-owned tools directory and add
that directory to PATH, or configure `withnav.serverPath` with its absolute path. In JSON, for
example: `"withnav.serverPath": "C:\\My Tools\\WithNav\\nav.exe"`. Do not add shell quotes around
the path, even when it contains spaces: the extension starts the LSP directly, without a shell.
Explicit settings still take precedence over the bundled executable, then PATH.

Install the Microsoft Visual C++ v14 x64 Redistributable if the executable reports missing
MSVC runtime DLLs. Redistributables are not bundled; clean-machine runtime verification remains
a release gate.

Windows artifacts are a **compile/packaging preview**, not a supported runtime.
Session ownership is disabled, so `nav run`, TUI and LSP sessions cannot start.
Credentials-file, settings and grants persistence, flows, scratch, native file tools,
shell execution (even with Git Bash), and browser auto-open are also disabled.
`nav doctor` and API keys supplied through environment variables work.
See [#8](https://github.com/PixmaNts/withnav/issues/8) for the backend plan.

Build on Windows with the MSVC Rust toolchain and Visual Studio C++ build tools. The existing
packaging script can run under Git Bash with a prebuilt binary:

```sh
cargo build --release --locked --bin nav --target x86_64-pc-windows-msvc
NAV_BINARY="$PWD/target/x86_64-pc-windows-msvc/release/nav.exe" scripts/package-vsix.sh win32-x64
```

On Linux, cargo-xwin can check the MSVC code and native dependencies when its SDK/CRT and C
toolchain are installed:

```sh
cargo xwin check --workspace --all-targets --locked --target x86_64-pc-windows-msvc
cargo xwin clippy --workspace --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings
```

Cross-checking is not runtime validation. Native Windows tests and a clean standard-user machine
must verify the preview's refusals, console behavior and bundled VSIX activation.
Windows ARM64, MSI/winget packages, signing and native PowerShell tooling are not provided.
