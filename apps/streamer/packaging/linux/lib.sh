# Shared helpers of the Linux packaging scripts. Source this file; do not run it.

# tauri-cli watches Cargo.toml and aborts with "Too many open files" when it cannot
# get an inotify instance. With rootless podman, the container uses the inotify
# instances of the host user (fs.inotify.max_user_instances, often 128).
# inotifywait: exit 2 is the 1-second timeout, exit 1 is an error.
check_inotify() {
  local status=0
  inotifywait -qq -t 1 / 2>/dev/null || status=$?
  if [[ $status -eq 1 ]]; then
    echo "No free inotify instance. Raise the limit on the host, for example:" >&2
    echo "  sudo sysctl fs.inotify.max_user_instances=1024" >&2
    exit 1
  fi
}

# Copies the repository from /src (read-only) to $1, so a build does not write into
# the host checkout. tar keeps the modification times, so cargo reuses the cached
# build in /build/target.
copy_sources() {
  local dest=$1
  mkdir -p "$dest"
  tar -C /src -cf - \
    --exclude=./.git \
    --exclude=node_modules \
    --exclude=.next \
    --exclude=.turbo \
    --exclude=./apps/streamer/target \
    --exclude=./apps/streamer/dist \
    --exclude=./apps/streamer/src-tauri/gen \
    . | tar -C "$dest" -xf -
}
