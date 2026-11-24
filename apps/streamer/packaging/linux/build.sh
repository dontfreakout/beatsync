#!/usr/bin/env bash
# Builds the Linux bundles (AppImage, deb, rpm) of Beatsync Streamer in an
# Ubuntu 24.04 container, so that they run on glibc 2.39 and later.
#
# A plain `bun run app:build` uses the libraries of this machine: on a new
# distribution the AppImage then fails on older ones with
# "version `GLIBC_2.xx' not found". Use this script for bundles you give to others.
#
# Output: apps/streamer/target/linux-release/
# Engine: podman or docker; set CONTAINER_ENGINE to choose one.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
streamer=$(cd "$here/../.." && pwd)
repo=$(cd "$streamer/../.." && pwd)
out=$streamer/target/linux-release

engine=${CONTAINER_ENGINE:-}
if [[ -z $engine ]]; then
  engine=$(command -v podman || command -v docker || true)
fi
if [[ -z $engine ]]; then
  echo "podman or docker is necessary" >&2
  exit 1
fi

image=beatsync-streamer-build:ubuntu24.04
"$engine" build --target build -t "$image" "$here"

name=beatsync-streamer-build-$$
trap '"$engine" rm -f "$name" >/dev/null 2>&1 || true' EXIT

# The named volumes keep the cargo registry, the cargo build, and the bun and
# Tauri downloads between runs. label=disable: no SELinux relabel of the checkout.
"$engine" run --name "$name" \
  --security-opt label=disable \
  -v "$repo:/src:ro" \
  -v beatsync-streamer-cargo:/opt/cargo/registry \
  -v beatsync-streamer-target:/build/target \
  -v beatsync-streamer-cache:/root/.cache \
  -e BUN_INSTALL_CACHE_DIR=/root/.cache/bun \
  "$image" bash /src/apps/streamer/packaging/linux/build-in-container.sh

# docker/podman cp writes the files as the user who runs this script.
rm -rf "$out"
mkdir -p "$out"
"$engine" cp "$name:/build/out/." "$out/"
echo "Bundles in $out:"
ls -1 "$out"
