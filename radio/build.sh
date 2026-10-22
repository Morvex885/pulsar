#!/bin/sh
# Builds the Zigbee firmware for the Station Midi's radio: radio/out/zigbee-coordinator.ybl and
# radio/out/zigbee-router.ybl.
# Fetches (once) the pinned Telink Zigbee SDK into radio/src/ and applies sdk.patch, and the Telink
# RISC-V toolchain (32-bit x86 Linux binaries: needs the i386 C library, e.g.
# `dpkg --add-architecture i386 && apt-get install libc6:i386`) into radio/toolchain/, unless
# TELINK_TC points to its bin/riscv32-elf- prefix. Needs git, curl, unzip, patch, make and zig.
set -eu
cd "$(dirname "$0")"

SDK_URL=https://github.com/telink-semi/telink_zigbee_sdk.git
SDK_REF=7cf1d6dbac6497505d649bd03342efe81f1a8212
TC_URL=http://wiki.telink-semi.cn/tools_and_sdk/Tools/IDE/telink_riscv_linux_toolchain.zip
TC_SHA256=bfe5dd517b268512fed5b9db956d243c64750c62a1a735ac41bbf0dcb6094489

sdk=src/telink_zigbee_sdk
# .fetched records the ref and the patch it was made with: a changed sdk.patch fetches again.
stamp="$SDK_REF $(sha256sum sdk.patch | cut -d' ' -f1)"
if [ "$(cat "$sdk/.fetched" 2>/dev/null)" != "$stamp" ]; then
  rm -rf "$sdk"
  git init -q "$sdk"
  git -C "$sdk" remote add origin "$SDK_URL"
  git -C "$sdk" fetch -q --depth 1 origin "$SDK_REF"
  git -C "$sdk" -c advice.detachedHead=false checkout -q FETCH_HEAD
  patch -s -p1 -d "$sdk/tl_zigbee_sdk" <sdk.patch
  echo "$stamp" >"$sdk/.fetched"
  echo "fetched telink_zigbee_sdk @ $SDK_REF"
fi

if [ -z "${TELINK_TC:-}" ]; then
  if [ ! -e toolchain/.fetched ]; then
    rm -rf toolchain
    mkdir -p toolchain
    curl -fsSL -o toolchain/tc.zip "$TC_URL"
    echo "$TC_SHA256  toolchain/tc.zip" | sha256sum -c - >/dev/null
    unzip -q toolchain/tc.zip -d toolchain
    rm toolchain/tc.zip
    mv toolchain/telink_riscv_linux_toolchain/nds32le-elf-mculib-v5f toolchain/
    rm -rf toolchain/telink_riscv_linux_toolchain
    touch toolchain/.fetched
  fi
  TELINK_TC=$PWD/toolchain/nds32le-elf-mculib-v5f/bin/riscv32-elf-
fi

make -j"$(nproc)" SDK="$sdk/tl_zigbee_sdk" TC="$TELINK_TC" ${ZIG:+ZIG="$ZIG"}
