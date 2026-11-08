#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
binary="$root/target/release/gitviews"
licenses="$root/dist/third-party-licenses"
archive="$root/dist/GitViews-linux-x64.tar.gz"

if [[ ! -f "$binary" ]]; then
  printf 'Missing Linux release binary: %s\n' "$binary" >&2
  exit 1
fi
if [[ ! -d "$licenses" ]] || [[ -z "$(find "$licenses" -type f -print -quit)" ]]; then
  printf 'Missing or empty third-party license directory: %s\n' "$licenses" >&2
  exit 1
fi

stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT
package="$stage/GitViews-linux-x64"
mkdir -p "$package"
cp -- "$binary" "$package/gitviews"
chmod 755 "$package/gitviews"
cp -R -- "$licenses" "$package/third-party-licenses"
cat > "$package/README.txt" <<'EOF'
GitViews for Linux x86_64

Built on Ubuntu 24.04. This executable dynamically links system libraries and
requires a compatible Linux system. Run `ldd ./gitviews` to inspect dependencies.
A graphical Wayland or X11 session and a working Vulkan driver are required.
Install the appropriate graphics driver and missing runtime libraries for your
distribution before starting the application:

    ./gitviews

Third-party license texts are in third-party-licenses/.
EOF

tar -czf "$archive" -C "$stage" GitViews-linux-x64
(cd -- "$root/dist" && sha256sum -- "${archive##*/}" > "${archive##*/}.sha256")
printf 'Created %s and %s.sha256\n' "$archive" "$archive"
