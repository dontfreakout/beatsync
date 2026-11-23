#!/usr/bin/env bash
# Runs the streamer-core unit tests and the PipeWire integration tests in an
# Ubuntu 24.04 container with PipeWire 1.0.5 and WirePlumber 0.4.17, the oldest
# versions the app supports. Exit status: that of the test run.
# Engine: podman or docker; set CONTAINER_ENGINE to choose one.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../../../.." && pwd)

engine=${CONTAINER_ENGINE:-}
if [[ -z $engine ]]; then
  engine=$(command -v podman || command -v docker || true)
fi
if [[ -z $engine ]]; then
  echo "podman or docker is necessary" >&2
  exit 1
fi

image=beatsync-streamer-test:ubuntu24.04
"$engine" build --target test -t "$image" "$here"

# Same volumes as build.sh: the cargo build is shared with the bundle build.
exec "$engine" run --rm \
  --security-opt label=disable \
  -v "$repo:/src:ro" \
  -v beatsync-streamer-cargo:/opt/cargo/registry \
  -v beatsync-streamer-target:/build/target \
  -v beatsync-streamer-cache:/root/.cache \
  "$image" bash /src/apps/streamer/packaging/linux/test-in-container.sh
