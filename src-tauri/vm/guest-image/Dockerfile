# OpenApp app-VM guest image (contract §16/§17, §21 G3). Build: `node src-tauri/vm/guest-image/build.ts`
# (default builder: a microsandbox VM, no Docker needed; `--builder docker` uses this file).
# Debian stable slim (trixie, glibc), arm64, pinned by digest; contents in setup.sh (shared by both
# builders). Bump all pins together and change IMAGE_VERSION in build.ts and GUEST_IMAGE_VERSION in
# src-tauri/src/vm.rs. Harness CLIs (Codex, Pi …) are NOT in the image: the kernel installs them pinned
# into the /opt/openapp volume on first use, through the kernel egress proxy.
FROM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f

COPY setup.sh /tmp/openapp-setup.sh
RUN sh /tmp/openapp-setup.sh && rm /tmp/openapp-setup.sh

# Egress goes only through the kernel proxy (HTTP(S)_PROXY is set per process by the kernel).
# Node's built-in fetch honours it with NODE_USE_ENV_PROXY=1 (Node >= 24).
ENV LANG=C.UTF-8 \
    NPM_CONFIG_UPDATE_NOTIFIER=false \
    NPM_CONFIG_FUND=false \
    NODE_USE_ENV_PROXY=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1

LABEL org.opencontainers.image.title="openapp-guest" \
      org.opencontainers.image.version="0.9.2" \
      org.opencontainers.image.base.name="debian:trixie-slim"
