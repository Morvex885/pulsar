#!/bin/sh
# Builds static (musl) dropbear + OpenSSH sftp-server with zig into ssh/out/<arch>/ for
# aarch64 (Station Midi) and armhf (Station Lite 2).
# Usage: ssh/build.sh            (host tools: zig >= 0.13, curl, make; ZIG=/path/to/zig to override)
set -eu
DROPBEAR_VER=2025.88
DROPBEAR_SHA256=783f50ea27b17c16da89578fafdb6decfa44bb8f6590e5698a4e4d3672dc53d4
OPENSSH_VER=10.0p2
OPENSSH_SHA256=021a2e709a0edf4250b1256bd5a9e500411a90dddabea830ed59cef90eb9d85c
ZIG=${ZIG:-zig}

HERE=$(cd "$(dirname "$0")" && pwd)
DL=$HERE/out/src
mkdir -p "$DL"

fetch() { # url sha256 file
  [ -f "$3" ] || { curl -fsSL -o "$3.part" "$1" && mv "$3.part" "$3"; }
  echo "$2  $3" | sha256sum -c --quiet -
}
fetch "https://matt.ucc.asn.au/dropbear/releases/dropbear-$DROPBEAR_VER.tar.bz2" "$DROPBEAR_SHA256" \
  "$DL/dropbear-$DROPBEAR_VER.tar.bz2"
fetch "https://cdn.openbsd.org/pub/OpenBSD/OpenSSH/portable/openssh-$OPENSSH_VER.tar.gz" "$OPENSSH_SHA256" \
  "$DL/openssh-$OPENSSH_VER.tar.gz"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

build() { # arch zig-target autoconf-host [cpu flags]
  arch=$1 host=$3
  out=$HERE/out/$arch
  mkdir -p "$out" "$WORK/$arch"
  export CC="$ZIG cc -target $2 ${4:-}" AR="$ZIG ar" RANLIB="$ZIG ranlib"

  # dropbear: one multi-call binary; start.sh creates the dropbear/dropbearkey/scp/dbclient links.
  # Everything lives in /data/dropbear (rootfs is read-only).
  tar -xjf "$DL/dropbear-$DROPBEAR_VER.tar.bz2" -C "$WORK/$arch"
  cd "$WORK/$arch/dropbear-$DROPBEAR_VER"
  cat >localoptions.h <<'EOF'
#define SFTPSERVER_PATH "/data/dropbear/sftp-server"
#define DROPBEAR_PATH_SSH_PROGRAM "/data/dropbear/dbclient"
#define DEFAULT_ROOT_PATH "/usr/sbin:/usr/bin:/sbin:/bin:/data/dropbear"
#define DEFAULT_PATH "/usr/bin:/bin:/data/dropbear"
EOF
  LDFLAGS=-s ./configure --host="$host" --enable-static --disable-zlib --disable-wtmp \
    --disable-lastlog >configure.log 2>&1 || { tail -20 configure.log; exit 1; }
  make -j"$(nproc)" PROGRAMS="dropbear dropbearkey scp dbclient" MULTI=1 STATIC=1 >build.log 2>&1 \
    || { tail -20 build.log; exit 1; }
  cp dropbearmulti "$out/dropbearmulti"

  # sftp-server (scp in modern OpenSSH clients and sftp need it); no OpenSSL/zlib required.
  tar -xzf "$DL/openssh-$OPENSSH_VER.tar.gz" -C "$WORK/$arch"
  cd "$WORK/$arch"/openssh-*/
  LDFLAGS="-static -s" ./configure --host="$host" --without-openssl --without-zlib \
    --libexecdir=/data/dropbear >configure.log 2>&1 || { tail -20 configure.log; exit 1; }
  make -j"$(nproc)" sftp-server >build.log 2>&1 || { tail -20 build.log; exit 1; }
  cp sftp-server "$out/sftp-server"

  [ -s "$out/dropbearmulti" ] && [ -s "$out/sftp-server" ] || { echo "$arch: empty binaries" >&2; exit 1; }
  ls -l "$out/dropbearmulti" "$out/sftp-server"
}

build aarch64 aarch64-linux-musl aarch64-linux-musl
build armhf arm-linux-musleabihf arm-linux-musleabihf -mcpu=cortex_a53
