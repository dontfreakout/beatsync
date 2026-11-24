#!/usr/bin/env bash
# Checks that the container-built AppImage starts on an official Linux Mint image
# and reaches "audio backend ready". Build the bundles first: bun run app:build:linux
# Usage: check-mint.sh [IMAGE]   (default: docker.io/linuxmintd/mint22.3-amd64)
# Exit status: 0 when the app started, 1 otherwise.
# Engine: podman or docker; set CONTAINER_ENGINE to choose one.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
streamer=$(cd "$here/../.." && pwd)
image=${1:-docker.io/linuxmintd/mint22.3-amd64}

engine=${CONTAINER_ENGINE:-}
if [[ -z $engine ]]; then
  engine=$(command -v podman || command -v docker || true)
fi
if [[ -z $engine ]]; then
  echo "podman or docker is necessary" >&2
  exit 1
fi

shopt -s nullglob
appimages=("$streamer"/target/linux-release/*.AppImage)
if [[ ${#appimages[@]} -ne 1 ]]; then
  echo "Expected one AppImage in $streamer/target/linux-release, found ${#appimages[@]}." >&2
  echo "Run: bun run app:build:linux" >&2
  exit 1
fi

# label=disable: no SELinux relabel of the mounted files.
exec "$engine" run --rm \
  --security-opt label=disable \
  -v "$here:/packaging:ro" \
  -v "${appimages[0]}:/beatsync.AppImage:ro" \
  "$image" bash /packaging/check-start-in-container.sh
