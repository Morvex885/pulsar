#!/bin/sh
# One-time fetch of the Sendspin client library and its dependencies into third_party/src/.
# Every dependency is pinned to an exact commit/tag (the versions sendspin-cpp's own host build
# uses, cmake/host.cmake). Re-running is a no-op for checkouts that already exist.
set -eu
cd "$(dirname "$0")"
mkdir -p src

# name  url  ref  [submodules...]
fetch() {
  name=$1 url=$2 ref=$3
  shift 3
  dir=src/$name
  if [ -e "$dir/.fetched" ]; then
    return
  fi
  rm -rf "$dir"
  git init -q "$dir"
  git -C "$dir" remote add origin "$url"
  git -C "$dir" fetch -q --depth 1 origin "$ref"
  git -C "$dir" -c advice.detachedHead=false checkout -q FETCH_HEAD
  for sm in "$@"; do
    git -C "$dir" submodule update -q --init --depth 1 "$sm"
  done
  git -C "$dir" rev-parse HEAD >"$dir/.fetched"
  echo "fetched $name @ $(cat "$dir/.fetched")"
}

fetch sendspin-cpp https://github.com/Sendspin/sendspin-cpp.git 985fba57afb0462b520ab9ce974d1860858ed685
fetch ArduinoJson https://github.com/bblanchon/ArduinoJson.git refs/tags/v7.4.1
fetch noise-c https://github.com/esphome-libs/noise-c.git refs/tags/v0.1.13
fetch IXWebSocket https://github.com/machinezone/IXWebSocket.git refs/tags/v12.0.1
fetch micro-flac https://github.com/esphome-libs/micro-flac.git refs/tags/v0.3.0 lib/micro-ogg-demuxer
fetch micro-opus https://github.com/esphome-libs/micro-opus.git refs/tags/v0.3.5 lib/opus lib/micro-ogg-demuxer
