#!/usr/bin/env bash
# Builds one VS Code package into dist/.
#   linux-x64 | linux-arm64: platform package with a static `nav` at bin/nav
#   win32-x64:               native MSVC package with bin/nav.exe
#   universal:               no binary; the extension uses `nav` on PATH or withnav.serverPath
# NAV_BINARY=<path> reuses a prebuilt binary instead of building one with cargo-zigbuild.
# Prebuilt binaries on foreign architectures require explicit version metadata:
#   --nav-binary-version <version>, NAV_BINARY_VERSION=<version>, or a sibling <binary>.version file.
set -euo pipefail

variant=""
nav_binary_version="${NAV_BINARY_VERSION:-}"

usage() {
  echo "usage: $0 [--nav-binary-version <version>] <linux-x64|linux-arm64|win32-x64|universal>" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --nav-binary-version)
      [ $# -ge 2 ] || usage
      nav_binary_version="$2"
      shift 2
      ;;
    --nav-binary-version=*)
      nav_binary_version="${1#*=}"
      shift
      ;;
    -*)
      usage
      ;;
    *)
      if [ -z "$variant" ]; then
        variant="$1"
        shift
      else
        usage
      fi
      ;;
  esac
done

[ -n "$variant" ] || usage

executable=nav
case "$variant" in
  linux-x64) triple=x86_64-unknown-linux-musl ;;
  linux-arm64) triple=aarch64-unknown-linux-musl ;;
  win32-x64) triple=x86_64-pc-windows-msvc; executable=nav.exe ;;
  universal) triple= ;;
  *) usage ;;
esac

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ext="$root/vscode-withnav"
dist="$root/dist"
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
  target_dir="$(cd "$CARGO_TARGET_DIR" 2>/dev/null && pwd || echo "$CARGO_TARGET_DIR")"
else
  target_dir="$root/target"
fi

# A stale bin/nav would leak into a universal package, so clear it on every exit too.
trap 'rm -rf "$ext/bin"' EXIT
rm -rf "$ext/bin"
mkdir -p "$dist"

# Captures WITHNAV_VERSION from cargo's root package build-script execution record.
cargo_version_parser='
const readline = require("readline");
const rl = readline.createInterface({ input: process.stdin, terminal: false });
let version = null;
function isWithnav(pkg) {
  if (!pkg) return false;
  if (pkg.includes("#")) {
    const f = pkg.split("#")[1];
    return f.startsWith("withnav@") || f === "withnav";
  }
  return pkg.startsWith("withnav ") || pkg === "withnav";
}
rl.on("line", (line) => {
  if (!line.startsWith("{")) return;
  try {
    const obj = JSON.parse(line);
    if (obj.reason === "compiler-message" && obj.message && obj.message.rendered) {
      process.stderr.write(obj.message.rendered);
    } else if (obj.reason === "build-script-executed" && isWithnav(obj.package_id)) {
      const entry = obj.env && obj.env.find(e => e[0] === "WITHNAV_VERSION");
      if (entry) version = entry[1];
    }
  } catch {}
});
rl.on("close", () => {
  if (version) {
    process.stdout.write(version);
  } else {
    console.error("error: cargo did not report WITHNAV_VERSION for package withnav (no build-script-executed env entry)");
    process.exit(1);
  }
});
'

if [ -n "$triple" ]; then
  binary="${NAV_BINARY:-}"
  if [ -n "$binary" ]; then
    [ -f "$binary" ] || { echo "binary not found: $binary" >&2; exit 1; }
    if [ -x "$binary" ] && raw_ver="$("$binary" --version 2>/dev/null)"; then
      version="$(printf '%s\n' "$raw_ver" | awk '{print $2; exit}' | tr -d '\r\n')"
    elif [ -n "$nav_binary_version" ]; then
      version="$(printf '%s' "$nav_binary_version" | tr -d '[:space:]')"
    elif [ -f "${binary}.version" ]; then
      version="$(tr -d '[:space:]' < "${binary}.version")"
    elif [ -f "${binary%.exe}.version" ]; then
      version="$(tr -d '[:space:]' < "${binary%.exe}.version")"
    else
      echo "error: prebuilt binary '$binary' cannot be executed on this host to read its version." >&2
      echo "Provide explicit version metadata using --nav-binary-version <version>, NAV_BINARY_VERSION, or a sibling '$binary.version' file." >&2
      exit 1
    fi
  else
    # Symbols are stripped to keep the package small; they are not needed at runtime.
    if [ "$variant" = win32-x64 ]; then
      version="$( (cd "$root" && cargo build --release --locked --bin nav --target "$triple" --message-format=json) | node -e "$cargo_version_parser" )"
    else
      version="$( (cd "$root" && CARGO_PROFILE_RELEASE_STRIP=symbols \
        cargo zigbuild --release --locked --bin nav --target "$triple" --message-format=json) | node -e "$cargo_version_parser" )"
    fi
    binary="$target_dir/$triple/release/$executable"
  fi
  [ -f "$binary" ] || { echo "binary not found: $binary" >&2; exit 1; }
  mkdir -p "$ext/bin"
  install -m 0755 "$binary" "$ext/bin/$executable"
else
  version="$( (cd "$root" && cargo check -p withnav --lib --message-format=json) | node -e "$cargo_version_parser" )"
fi

[ -n "$version" ] || { echo "error: failed to determine version" >&2; exit 1; }

node -e '
const v = process.argv[1];
const semver = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$/;
if (!semver.test(v)) {
  console.error(`error: invalid SemVer 2.0.0 version string: "${v}"`);
  process.exit(1);
}
' "$version"

cd "$ext"
[ -d node_modules ] || npm ci
if [ -n "$triple" ]; then
  out="$dist/withnav-$variant-$version.vsix"
  npx --no-install vsce package "$version" --no-update-package-json --allow-missing-repository --target "$variant" -o "$out"
else
  out="$dist/withnav-$version.vsix"
  npx --no-install vsce package "$version" --no-update-package-json --allow-missing-repository -o "$out"
fi
echo "$out"
