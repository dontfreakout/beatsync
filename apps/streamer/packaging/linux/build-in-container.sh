#!/usr/bin/env bash
# Runs inside the image of ./Dockerfile; build.sh starts it.
# Expects the repository at /src (read-only) and writes the bundles to /build/out.
set -euo pipefail

# Oldest glibc the bundles must run on: the glibc of the base image.
MAX_GLIBC=2.39

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

check_inotify
copy_sources /build/src

cd /build/src
# --ignore-scripts: the root prepare script installs git hooks, and there is no git here.
bun install --frozen-lockfile --ignore-scripts --filter streamer

cd apps/streamer
export CARGO_TARGET_DIR=/build/target
bun run tauri build

bundle=$CARGO_TARGET_DIR/release/bundle
shopt -s nullglob
artifacts=("$bundle"/appimage/*.AppImage "$bundle"/deb/*.deb "$bundle"/rpm/*.rpm)
if [[ ${#artifacts[@]} -eq 0 ]]; then
  echo "no bundles in $bundle" >&2
  exit 1
fi

bash packaging/linux/check-glibc.sh "$MAX_GLIBC" "${artifacts[@]}"

# K7: the AppImage must use the PipeWire of the target system. Bundled libpipewire,
# SPA plugins or WirePlumber parts would conflict with the system's session manager.
appimage=("$bundle"/appimage/*.AppImage)
if [[ ${#appimage[@]} -ne 1 ]]; then
  echo "expected one AppImage in $bundle/appimage" >&2
  exit 1
fi
appimage_dir=$(mktemp -d)
(cd "$appimage_dir" && "${appimage[0]}" --appimage-extract >/dev/null)
bundled=$(find "$appimage_dir" \( -name 'libpipewire-0.3.so*' -o -name 'libspa-*' \
  -o -type d -name spa-0.2 -o -name 'libwireplumber*' \))
if [[ -n $bundled ]]; then
  echo "FAIL: the AppImage contains PipeWire or WirePlumber parts:" >&2
  echo "$bundled" >&2
  exit 1
fi
rm -rf "$appimage_dir"
echo "OK: no PipeWire or WirePlumber libraries in the AppImage"

mkdir -p /build/out
cp "${artifacts[@]}" /build/out/
