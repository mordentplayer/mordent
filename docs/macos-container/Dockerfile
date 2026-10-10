# wrapper-lite container (WorldObservationLog/wrapper, `lite` branch).
#
# Build context = this directory, with three inputs staged into it (they are
# NOT committed here):
#   wrapper-lite-rootless  the x86-64 host launcher, built by the release
#                          workflow's `build-wrapper` job (CMake target
#                          wrapper_lite_rootless_exe) and shipped in the
#                          `wrapper-payload` artifact
#   rootfs/                the Android rootfs skeleton + the freshly built
#                          Android-layer binary at rootfs/system/bin/lite
#                          (also from `wrapper-payload`)
#   entrypoint.sh          committed here
#
# The `build-container` job in .github/workflows/release.yml does exactly that
# staging, then `docker build -t wrapper-lite:amd64 .` and `docker save | gzip`.
# For a local build, stage the same three inputs (e.g. from a local wrapper
# checkout: copy `wrapper-lite-rootless` and `rootfs/`, minus rootfs/data) and
# run the same docker build.
#
# linux/amd64 ONLY. The FairPlay payload in rootfs/system/bin/lite and every
# bionic lib in rootfs/system/lib64 is Android *x86-64* (the repo's CMakeLists
# uses x86_64-linux-android22-clang). There is no arm64 payload build, so a
# linux/arm64 image would only add a second emulation layer without removing
# the x86-64 one — see FINDINGS.md ("Architecture analysis"). On Apple Silicon
# this image runs under Docker Desktop / OrbStack amd64 emulation.
FROM debian:stable-slim

WORKDIR /app
COPY wrapper-lite-rootless /app/wrapper-lite-rootless
COPY rootfs /app/rootfs
COPY entrypoint.sh /app/entrypoint.sh

# rootfs/data is NOT in the image (see .dockerignore): login tokens are the
# user's own Apple session and must only ever live in the mounted volume.
RUN chmod +x /app/wrapper-lite-rootless /app/entrypoint.sh \
    && rm -rf /app/rootfs/data \
    && mkdir -p /app/rootfs/data /app/rootfs/proc /app/rootfs/dev

EXPOSE 12340
ENTRYPOINT ["/app/entrypoint.sh"]
