# Toolchain image for the Linux bundles (AppImage, deb, rpm) of Beatsync Streamer.
#
# An AppImage carries the GTK and WebKit libraries of the build system, but it uses
# the glibc of the target system. The glibc of the build system therefore sets the
# oldest system the bundles run on. Ubuntu 24.04 has glibc 2.39 (Linux Mint 22,
# Debian 13 and later have this or newer) and PipeWire 1.0, the oldest PipeWire
# the app supports. Do not move to a newer base without a reason.
#
# The image holds no sources: build.sh mounts the repository at run time.
FROM docker.io/oven/bun:1.4.4 AS bun

FROM docker.io/library/ubuntu:24.04 AS build

ARG RUST_VERSION=1.99.1

# Tauri prerequisites (tauri.app/start/prerequisites), PipeWire headers and clang
# for the pipewire crate, cmake for the bundled libopus, the tools of
# check-glibc.sh (binutils from build-essential, dpkg, rpm2cpio, cpio), and
# inotify-tools for the inotify check in build-in-container.sh.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential ca-certificates curl wget file pkg-config \
      libwebkit2gtk-4.1-dev libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev \
      libpipewire-0.3-dev clang libclang-dev cmake \
      rpm2cpio cpio inotify-tools \
 && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal --default-toolchain "$RUST_VERSION"

COPY --from=bun /usr/local/bin/bun /usr/local/bin/bun

# linuxdeploy and appimagetool are AppImages themselves. A container has no FUSE.
ENV APPIMAGE_EXTRACT_AND_RUN=1
