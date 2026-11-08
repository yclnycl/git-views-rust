#!/usr/bin/env bash
# Package the native release binary and its generated third-party licenses.
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "Usage: $0 <arm64|x64> [third-party-license-directory]" >&2
  exit 2
fi

case "$1" in
  arm64) native_arch=arm64 ;;
  x64) native_arch=x86_64 ;;
  *) echo "Unsupported macOS architecture: $1" >&2; exit 2 ;;
esac
package_arch=$1
project_root=$(cd "$(dirname "$0")/.." && pwd)
cd "$project_root"
binary=target/release/gitviews
licenses=${2:-dist/third-party-licenses}

if [[ $(uname -s) != Darwin ]]; then
  echo 'This packaging script requires macOS.' >&2
  exit 1
fi
if [[ ! -f $binary || ! -x $binary ]]; then
  echo "Missing executable: $binary; build it with cargo build --release first." >&2
  exit 1
fi
if [[ ! -d $licenses ]] || [[ -z $(find "$licenses" -type f -print -quit) ]]; then
  echo "Missing or empty generated third-party license directory: $licenses" >&2
  exit 1
fi

lipo -verify_arch "$native_arch" "$binary"
# A runner-local Homebrew dependency will not be present on the recipient's Mac.
# Permit only macOS system libraries/frameworks until other libraries are bundled.
dependencies=$(otool -L "$binary")
while IFS= read -r dependency; do
  case "$dependency" in
    /usr/lib/*|/System/Library/*) ;;
    *) echo "Unsupported macOS dynamic library dependency: $dependency" >&2; exit 1 ;;
  esac
done < <(printf '%s\n' "$dependencies" | tail -n +2 | awk '{print $1}')

version=$(awk '
  /^\[workspace\.package\]/ { in_package = 1; next }
  /^\[/ { in_package = 0 }
  in_package && /^version[[:space:]]*=/ { split($0, parts, "\""); print parts[2]; exit }
' Cargo.toml)
if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo 'Unable to determine the workspace version from Cargo.toml.' >&2
  exit 1
fi

mkdir -p dist
stage=$(mktemp -d "${TMPDIR:-/tmp}/gitviews-macos.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
mkdir -p "$stage/GitViews.app/Contents/MacOS"
cp "$binary" "$stage/GitViews.app/Contents/MacOS/gitviews"
chmod 755 "$stage/GitViews.app/Contents/MacOS/gitviews"
cp -R "$licenses" "$stage/third-party-licenses"
cat > "$stage/GitViews.app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>gitviews</string>
  <key>CFBundleIdentifier</key><string>com.gitviews.app</string>
  <key>CFBundleName</key><string>GitViews</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
EOF
plutil -lint "$stage/GitViews.app/Contents/Info.plist"
cat > "$stage/README.txt" <<'EOF'
GitViews for macOS

Extract the archive and open GitViews.app. Move it to Applications if desired.
This build is not signed or notarized. macOS Gatekeeper may prevent it from
opening; see Apple's instructions for opening an app from an unidentified developer.
The third-party-licenses directory contains dependency license information.
EOF
archive="GitViews-macos-$package_arch.tar.gz"
tar -C "$stage" -czf "${project_root}/dist/$archive" GitViews.app README.txt third-party-licenses
(cd dist && shasum -a 256 "$archive" > "$archive.sha256")
echo "Created dist/$archive and dist/$archive.sha256"
