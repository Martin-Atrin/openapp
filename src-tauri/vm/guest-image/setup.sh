#!/bin/sh
# OpenApp guest image contents (contract §21 G3), shared by both builders in build.ts (Docker RUN and
# the microsandbox builder VM). Runs as root on the pinned Debian base (see Dockerfile / build.ts).
# Pins: Debian packages come from the pinned base's suite (stable point releases only); Node by
# version + sha256 (official binaries in /usr/local, the same paths as the former node:alpine image).
set -eu
export DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8

NODE_VERSION=24.22.0
NODE_SHA256_arm64=fef00bb4d36e1c50104a9775a10359beee98f162b4104dd12472c75065c6a143
NODE_SHA256_amd64=cc2f601cee82d9a5a233302fe307abc600727035fd7d2c3a3571755989e95008

# Toolchain + virtual display (Xvfb + x11vnc + websockify/noVNC) + qemu-user (x86_64 binaries, opt-in).
apt-get update
apt-get install -y --no-install-recommends \
  bash bubblewrap build-essential ca-certificates curl e2fsprogs fd-find ffmpeg file git jq less \
  procps proxychains4 psmisc python3 python3-venv python3-pip ripgrep socat unzip xz-utils zstd \
  xvfb x11vnc novnc python3-websockify xauth x11-utils xterm \
  qemu-user binfmt-support

# Pi's built-in find tool runs `fd` (and grep runs `rg`, installed above); Debian names the binary fdfind.
ln -sf /usr/bin/fdfind /usr/local/bin/fd

# qemu-user ships ~30 targets (~450 MB); keep x86_64 + i386 (the "arch": "amd64" use case)
find /usr/bin -maxdepth 1 -name 'qemu-*' ! -name 'qemu-x86_64' ! -name 'qemu-i386' -delete

arch=$(dpkg --print-architecture)
case "$arch" in
  arm64) na=arm64; sum=$NODE_SHA256_arm64 ;;
  amd64) na=x64; sum=$NODE_SHA256_amd64 ;;
  *) echo "unsupported arch $arch" >&2; exit 1 ;;
esac
f=node-v$NODE_VERSION-linux-$na.tar.xz
curl -fsSLo /tmp/$f https://nodejs.org/dist/v$NODE_VERSION/$f
echo "$sum  /tmp/$f" | sha256sum -c -
tar -xJf /tmp/$f -C /usr/local --strip-components=1 --no-same-owner \
  --exclude='*/CHANGELOG.md' --exclude='*/README.md' --exclude='*/doc'
rm -f /tmp/$f
node --version
npm --version

# apt goes through the kernel egress proxy (actor `apt`; Debian mirrors are allowed by default, over
# HTTPS so CONNECT keeps connections alive). Package lists and downloaded .debs live on the persistent
# /opt/openapp volume, so packages a module needs reinstall offline and fast after each VM boot
# (the rootfs is fresh on every boot; dpkg state belongs to the image).
sed -i 's#http://deb.debian.org#https://deb.debian.org#g' /etc/apt/sources.list.d/debian.sources
cat > /etc/apt/apt.conf.d/80openapp-proxy <<'EOF'
Acquire::http::Proxy "http://apt:apt@127.0.0.1:7802";
Acquire::https::Proxy "http://apt:apt@127.0.0.1:7802";
Acquire::Retries "2";
EOF
cat > /etc/apt/apt.conf.d/81openapp-volume <<'EOF'
Dir::State::Lists "/opt/openapp/apt/lists";
Dir::Cache::Archives "/opt/openapp/apt/archives";
APT::Install-Recommends "false";
APT::Get::Assume-Yes "true";
EOF
rm -f /etc/apt/apt.conf.d/docker-clean

# Non-HTTP egress (IMAP, MTProto, MQTT, raw TCP): the kernel SOCKS5 proxy at 127.0.0.1:7803 (vsock-forwarded,
# same consent as the HTTP proxy). Programs with a proxy option get OPENAPP_SOCKS_URL=socks5h://<actor>:openapp@127.0.0.1:7803
# (set per process by the kernel); others run under proxychains4 (manifest "net": "socks" on a service or server).
# The kernel writes a per-actor copy of this file; guest-local addresses (127.0.0.0/8) stay direct.
cat > /etc/proxychains4.conf <<'EOF'
strict_chain
proxy_dns
remote_dns_subnet 224
tcp_read_time_out 15000
tcp_connect_time_out 8000
localnet 127.0.0.0/255.0.0.0
[ProxyList]
socks5 127.0.0.1 7803 guest openapp
EOF
rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*.deb /var/cache/apt/*.bin /var/log/apt /var/log/dpkg.log

# Kernel-installed CLIs, tools and their logins live on the volume. qemu-user: `qemu-x86_64 <bin>`
# (binfmt registration needs binfmt_misc in the guest kernel; the kernel wraps "arch": "amd64" commands).
mkdir -p /opt/openapp /workspace
git config --system safe.directory '*'
git config --system init.defaultBranch main
cat > /etc/openapp-guest <<EOF
image=openapp-guest
version=0.9.2
base=debian:trixie-slim
libc=glibc
node=$NODE_VERSION
EOF
