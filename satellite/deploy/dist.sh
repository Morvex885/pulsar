#!/bin/sh
# Assembles the release archive for one host OS: pulsar-install, both daemon builds, the Midi
# radio's firmware images, dropbear and the device scripts in the repository layout (pulsar-install
# finds them next to itself).
# Usage: satellite/deploy/dist.sh <out.zip|out.tar.gz> <pulsar-install binary>
#   (needs: make -C satellite; radio/build.sh; ssh/build.sh; the installer built for the target OS)
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
[ $# = 2 ] || { echo "usage: $0 <out.zip|out.tar.gz> <pulsar-install binary>" >&2; exit 1; }
case $1 in *.zip|*.tar.gz) ;; *) echo "unknown archive type: $1 (.zip or .tar.gz)" >&2; exit 1 ;; esac
[ -s "$2" ] || { echo "missing $2" >&2; exit 1; }
OUT=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
INST=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
P=$STAGE/pulsar
put() { # repo-relative path, mode
  [ -s "$ROOT/$1" ] || { echo "missing $1 (make -C satellite; radio/build.sh; ssh/build.sh)" >&2; exit 1; }
  mkdir -p "$P/$(dirname "$1")"
  cp "$ROOT/$1" "$P/$1"
  # CI artifacts lose permission bits.
  chmod "$2" "$P/$1"
}
for a in aarch64 armhf; do
  put satellite/build/$a/pulsar 755
  put ssh/out/$a/dropbearmulti 755
  put ssh/out/$a/sftp-server 755
done
put radio/out/zigbee-router.ybl 644
put radio/out/zigbee-coordinator.ybl 644
for f in boot.sh supervise.sh ctl.sh; do put satellite/deploy/$f 755; done
put satellite/deploy/asound.conf 644
put ssh/start.sh 755
put README.md 644
put LICENSE 644
cp "$INST" "$P/" && chmod 755 "$P/$(basename "$INST")"
rm -f "$OUT"
case $OUT in
  *.zip) (cd "$STAGE" && zip -qrX "$OUT" pulsar) ;;
  *) tar -czf "$OUT" -C "$STAGE" --owner=0 --group=0 --numeric-owner pulsar ;;
esac
echo "$OUT"
