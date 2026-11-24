#!/usr/bin/env bash
# Runs inside a Linux Mint image (started by check-mint.sh). Starts the AppImage
# under a headless X server and a headless PipeWire + WirePlumber session, and
# passes when the app prints "audio backend ready" and keeps running.
set -euo pipefail

appimage=/beatsync.AppImage
log=/tmp/streamer-app.log

# check: the part that runs inside the PipeWire session.
if [[ ${1:-} == check ]]; then
  # No FUSE in a container: the AppImage unpacks itself and runs from there.
  export APPIMAGE_EXTRACT_AND_RUN=1

  : >"$log"
  # The redirect sits inside xvfb-run: xvfb-run itself passes the stderr of its
  # command to its own stderr, so a redirect outside would leave the log empty.
  xvfb-run -a bash -c 'exec "$0" 2>"$1"' "$appimage" "$log" &
  app=$!
  trap 'pkill -P "$app" 2>/dev/null || true; kill "$app" 2>/dev/null || true' EXIT

  fail() {
    echo "FAIL: $1" >&2
    echo "--- app log" >&2
    cat "$log" >&2
    exit 1
  }

  bad='streamer-app: no audio backend|error while loading shared libraries|not found \(required by|panicked'
  for _ in $(seq 300); do
    if grep -Eq "$bad" "$log"; then fail "the app reported an error"; fi
    if grep -q 'streamer-app: audio backend ready' "$log"; then break; fi
    kill -0 "$app" 2>/dev/null || fail "the app exited before the audio backend was ready"
    sleep 0.1
  done
  grep -q 'streamer-app: audio backend ready' "$log" || fail "no audio backend within 30 s"

  sleep 5
  if grep -Eq "$bad" "$log"; then fail "the app reported an error"; fi
  kill -0 "$app" 2>/dev/null || fail "the app exited after the audio backend was ready"
  echo "OK: the app started and the audio backend is ready"
  exit 0
fi

export DEBIAN_FRONTEND=noninteractive
# The Mint package repository of a pre-release image can be gone (mint23: "alfa").
# apt-get update still loads the Ubuntu lists, and the install below needs only those.
apt-get update -qq || echo "apt-get update failed for some repositories; going on" >&2
apt-get install -y -qq --no-install-recommends \
  xvfb xauth pipewire wireplumber pipewire-bin dbus >/dev/null

echo "System: $(. /etc/os-release && echo "$PRETTY_NAME"), glibc $(ldd --version | head -n 1 | awk '{print $NF}')"
echo "PipeWire: $(pipewire --version | tail -n 1)"
echo "WirePlumber: $(wireplumber --version 2>&1 | head -n 3 | tr '\n' ' ')"

dbus-run-session -- bash /packaging/pipewire-session.sh bash "$0" check
