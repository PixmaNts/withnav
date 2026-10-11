#!/usr/bin/env bash
# Builds one VS Code package into dist/.
#   linux-x64 | linux-arm64: platform package with a static `nav` at bin/nav
#   universal:               no binary; the extension uses `nav` on PATH or withnav.serverPath
# NAV_BINARY=<path> reuses a prebuilt binary instead of building one with cargo-zigbuild.
set -euo pipefail

variant="${1:-}"
case "$variant" in
  linux-x64) triple=x86_64-unknown-linux-musl ;;
  linux-arm64) triple=aarch64-unknown-linux-musl ;;
  universal) triple= ;;
  *) echo "usage: $0 <linux-x64|linux-arm64|universal>" >&2; exit 2 ;;
esac

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ext="$root/vscode-withnav"
dist="$root/dist"
version="$(node -p "require('$ext/package.json').version")"

# A stale bin/nav would leak into a universal package, so clear it on every exit too.
trap 'rm -rf "$ext/bin"' EXIT
rm -rf "$ext/bin"
mkdir -p "$dist"

if [ -n "$triple" ]; then
  binary="${NAV_BINARY:-}"
  if [ -z "$binary" ]; then
    # Symbols are stripped to keep the package small; they are not needed at runtime.
    (cd "$root" && CARGO_PROFILE_RELEASE_STRIP=symbols \
      cargo zigbuild --release --locked --bin nav --target "$triple")
    binary="$root/target/$triple/release/nav"
  fi
  [ -f "$binary" ] || { echo "binary not found: $binary" >&2; exit 1; }
  mkdir -p "$ext/bin"
  install -m 0755 "$binary" "$ext/bin/nav"
fi

cd "$ext"
[ -d node_modules ] || npm ci
if [ -n "$triple" ]; then
  out="$dist/withnav-$variant-$version.vsix"
  npx --no-install vsce package --allow-missing-repository --target "$variant" -o "$out"
else
  out="$dist/withnav-$version.vsix"
  npx --no-install vsce package --allow-missing-repository -o "$out"
fi
echo "$out"
