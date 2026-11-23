#!/usr/bin/env bash
# Starts a headless PipeWire and WirePlumber, runs the command given as arguments,
# and stops both daemons. Start it under a session bus:
#   dbus-run-session -- bash pipewire-session.sh CMD...
# Exit status: that of CMD, or 1 when the session does not start.
set -euo pipefail

if [[ $# -eq 0 ]]; then
  echo "usage: pipewire-session.sh CMD..." >&2
  exit 1
fi

if [[ -z ${XDG_RUNTIME_DIR:-} ]]; then
  export XDG_RUNTIME_DIR=/tmp/xdg
  mkdir -p "$XDG_RUNTIME_DIR"
  chmod 700 "$XDG_RUNTIME_DIR"
fi

pipewire >/tmp/pipewire.log 2>&1 &
pw_pid=$!
wireplumber >/tmp/wireplumber.log 2>&1 &
wp_pid=$!
trap 'kill "$wp_pid" "$pw_pid" 2>/dev/null || true; wait 2>/dev/null || true' EXIT

fail() {
  echo "PipeWire session: $1" >&2
  echo "--- /tmp/pipewire.log" >&2
  tail -n 30 /tmp/pipewire.log >&2 || true
  echo "--- /tmp/wireplumber.log" >&2
  tail -n 30 /tmp/wireplumber.log >&2 || true
  exit 1
}

# wait_for DESCRIPTION CMD...: runs CMD every 0.1 s, up to 10 s.
wait_for() {
  local what=$1
  shift
  for _ in $(seq 100); do
    "$@" >/dev/null 2>&1 && return 0
    kill -0 "$pw_pid" 2>/dev/null || fail "pipewire exited"
    kill -0 "$wp_pid" 2>/dev/null || fail "wireplumber exited"
    sleep 0.1
  done
  fail "timeout: $what"
}

wireplumber_client() {
  pw-cli ls Client | grep -q 'application.name = "WirePlumber"'
}

standin_is_default() {
  pw-metadata -n default 0 default.audio.sink | grep -q beatsync-standin-output
}

wait_for "WirePlumber client" wireplumber_client

pw-cli create-node adapter '{ factory.name = support.null-audio-sink node.name = beatsync-standin-output node.description = "Stand-in output" media.class = Audio/Sink audio.position = [ FL FR ] priority.session = 1000 object.linger = true }' >/dev/null

wait_for "default sink beatsync-standin-output" standin_is_default

status=0
"$@" || status=$?
exit "$status"
