#!/bin/sh
# One-time build of the link-time sysroot: alsa-lib headers + libasound.so for one target.
# Only used for linking; at runtime the daemon uses the firmware's own /usr/lib/libasound.so.2.
# Symbol versions must match the firmware's library: the Midi's alsa-lib is versioned (the
# binaries import ALSA_0.9* only, which every alsa-lib 1.x provides), the Lite 2's 1.2.1.2 is
# built without versions (pass --with-versioned=no).
# Usage: third_party/sysroot.sh <sysroot dir> "<zig target> [cpu flags]" <autoconf host> [configure flags]
set -eu
OUT=$(realpath -m "$1")
TARGET=$2
HOST=$3
EXTRA=${4:-}
ZIG=${ZIG:-zig}
VER=1.2.10
SHA256=c86a45a846331b1b0aa6e6be100be2a7aef92efd405cf6bac7eef8174baa920e
SRC=$(cd "$(dirname "$0")" && pwd)/src
TARBALL=$SRC/alsa-lib-$VER.tar.bz2

mkdir -p "$SRC"
if [ ! -f "$TARBALL" ]; then
  # Upstream first, then the BLFS mirror of the same tarball (the sha256 check below covers both).
  for url in "https://www.alsa-project.org/files/pub/lib/alsa-lib-$VER.tar.bz2" \
             "https://ftp.osuosl.org/pub/blfs/conglomeration/alsa-lib/alsa-lib-$VER.tar.bz2"; do
    curl -fsSL --retry 3 --retry-all-errors -o "$TARBALL.part" "$url" && break
    rm -f "$TARBALL.part"
  done
  [ -f "$TARBALL.part" ] || { echo "alsa-lib $VER: download failed" >&2; exit 1; }
  mv "$TARBALL.part" "$TARBALL"
fi
echo "$SHA256  $TARBALL" | sha256sum -c --quiet -

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
tar -xjf "$TARBALL" -C "$WORK"
cd "$WORK/alsa-lib-$VER"
# Symbol versions are kept (ALSA_0.9 … like the firmware's build); lld rejects alsa-lib's version
# script entries for features that are not compiled in unless --undefined-version is given.
LDFLAGS="-Wl,--undefined-version"
CC="$ZIG cc -target $TARGET" AR="$ZIG ar" RANLIB="$ZIG ranlib" LDFLAGS="$LDFLAGS" \
  ./configure --host="$HOST" --prefix=/ --disable-python $EXTRA >configure.log 2>&1 \
  || { tail -20 configure.log; exit 1; }
case " $EXTRA " in
  *" --with-versioned=no "*)
    # Without alsa's version script the library would also export zig's compiler-rt helpers
    # (__aeabi_uldivmod, ...); the daemon would then import them from libasound, which the
    # firmware's copy does not provide. Export only the ALSA API, unversioned.
    printf '{ global: snd_*; _snd_*; local: *; };\n' >exports.map
    LDFLAGS="$LDFLAGS -Wl,--version-script=$PWD/exports.map" ;;
esac
make -j"$(nproc)" -C include >/dev/null
make -j"$(nproc)" -C src LDFLAGS="$LDFLAGS" >build.log 2>&1 || { tail -20 build.log; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT/lib"
make -C include install DESTDIR="$OUT" includedir=/include >/dev/null
cp -P src/.libs/libasound.so src/.libs/libasound.so.2 src/.libs/libasound.so.2.0.0 "$OUT/lib/"
echo "sysroot: alsa-lib $VER -> $OUT"
