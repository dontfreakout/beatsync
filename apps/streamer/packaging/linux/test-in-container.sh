#!/usr/bin/env bash
# Runs inside the test image of ./Dockerfile; test-pipewire.sh starts it.
# Expects the repository at /src (read-only).
set -euo pipefail

# Tests to skip: each entry is `--skip <test name>` plus a comment with the reason
# (spec 8.4 rules; only container noise may be listed).
SKIP=()

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

check_inotify
copy_sources /build/src

cd /build/src/apps/streamer
export CARGO_TARGET_DIR=/build/target

pipewire --version
wireplumber --version

dbus-run-session -- bash packaging/linux/pipewire-session.sh bash -c 'cargo test -p streamer-core && cargo test -p streamer-core -- --ignored pipewire "$@"' _ "${SKIP[@]}"
