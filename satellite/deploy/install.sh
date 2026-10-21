#!/usr/bin/env bash
# Finds rooted Yandex Stations on the local network and installs or updates pulsar.
#
#   satellite/deploy/install.sh [--list] [--scan] [--ssh] [--no-enable] [--yes] [ip[:port] ...]
#
# Without addresses: finds Yandex Stations via mDNS (avahi-browse) or, with --scan or without
# avahi, by scanning the /24 of every default-route interface for adb (port 5555); connects,
# identifies the model and offers the supported, rooted stations found. Per station:
# - copies the daemon + scripts to /data/pulsar (scripts via .new + mv; when the binary
#   differs from the installed one, the installed one is kept as .prev; an identical one is not
#   copied and leaves .prev alone);
# - --ssh also installs dropbear (static build from ssh/build.sh) to /data/dropbear;
# - adds the boot hooks to /data/quasar/rootkeeper.sh (backup: rootkeeper.sh.bak-<timestamp>),
#   placed before an endless `while` loop if the script has one, otherwise appended;
# - enables the satellite (stops the Yandex stack) unless --no-enable; an already running
#   satellite is restarted with the new binary and, after ~10 s, checked (process alive, no crash
#   report in the log); on failure the previous binary is put back (asked, or automatic with --yes).
# Missing build outputs are built first. Nothing outside /data is modified. Re-running is safe.
# --list only shows what was found; --yes installs to every station found without asking.
# --scan may reach other Android devices on the network, which can show an adb authorization prompt.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
SAT=$ROOT/satellite
D=/data/pulsar
RK=/data/quasar/rootkeeper.sh
HOOK_SAT='[ -x /data/pulsar/boot.sh ] && /data/pulsar/boot.sh </dev/null >/dev/null 2>&1 &'
HOOK_SSH='[ -x /data/dropbear/start.sh ] && /data/dropbear/start.sh </dev/null >/dev/null 2>&1 &'

# Devicetree codename -> name / userland ABI (build/<arch>/). Only models listed in ARCH_OF get
# installed: the daemon drives model-specific hardware (mic array + VQE preset, LEDs, clock, inputs).
declare -A MODEL_NAME=([cucumber]="Yandex Station Midi" [plum]="Yandex Station Lite 2")
declare -A ARCH_OF=([cucumber]=aarch64 [plum]=armhf)
declare -A ARCH_FOR=()  # serial -> arch of the probed station

SSH=0 ENABLE=1 YES=0 LIST=0 SCAN=0
TARGETS=()
for a in "$@"; do
  case $a in
    --ssh) SSH=1 ;;
    --no-enable) ENABLE=0 ;;
    -y|--yes) YES=1 ;;
    --list) LIST=1 ;;
    --scan) SCAN=1 ;;
    -h|--help) sed -n '2,17p' "$0" | cut -c3-; exit 0 ;;
    -*) echo "unknown option $a" >&2; exit 1 ;;
    *:*) TARGETS+=("$a") ;;
    *) TARGETS+=("$a:5555") ;;
  esac
done

# adb shell on these firmwares always exits 0 and adds \r: carry the remote status in the output.
sh_() { # serial command
  local out rc
  out=$(adb -s "$1" shell "$2
echo __rc=\$?" 2>/dev/null | tr -d '\r') || return 1
  [[ $out == *__rc=* ]] || return 1
  rc=${out##*__rc=}
  out=${out%__rc=*}
  [ -z "$out" ] || printf '%s' "$out"
  return "$rc"
}

build() {
  echo "== build"
  make -C "$SAT" -j"$(nproc)" >/dev/null
  if [ $SSH = 1 ] && { ! [ -s "$ROOT/ssh/out/aarch64/dropbearmulti" ] || ! [ -s "$ROOT/ssh/out/armhf/dropbearmulti" ]; }; then
    "$ROOT/ssh/build.sh" >/dev/null
  fi
}

default_nets() { # a.b.c for the IPv4 address of each default-route interface
  local dev
  for dev in $(ip -4 route show default | awk '{for (i = 1; i < NF; i++) if ($i == "dev") print $(i + 1)}' | sort -u); do
    ip -4 -o addr show dev "$dev" scope global | awk '{split($4, a, "/"); split(a[1], o, "."); print o[1] "." o[2] "." o[3]}'
  done | sort -u
}

scan() { # prints ip:5555 for hosts with an open adb port
  local net i
  for net in $(default_nets); do
    echo "scanning $net.0/24 for adb..." >&2
    for i in $(seq 1 254); do
      (timeout 0.5 bash -c "exec 3<>/dev/tcp/$net.$i/5555" 2>/dev/null && echo "$net.$i:5555") &
    done
  done
  wait
}

# Prints ip:5555 for Yandex Stations announced over mDNS: stock firmware as _yandexio._tcp,
# already converted ones as ESPHome devices named yandex-*. Only these get an `adb connect`, so
# other Android devices on the network are left alone.
mdns() {
  echo "looking for stations via mDNS..." >&2
  {
    avahi-browse -rpt _yandexio._tcp 2>/dev/null | awk -F';' '$1 == "=" && $3 == "IPv4" {print $8}'
    avahi-browse -rpt _esphomelib._tcp 2>/dev/null | awk -F';' '$1 == "=" && $3 == "IPv4" && $4 ~ /^yandex-/ {print $8}'
  } | sort -u | sed 's/$/:5555/'
}

# Fills FOUND (installable serials) and prints one status line per probed device.
FOUND=()
probe() { # serial connected_by_us
  local s=$1 ours=$2 compat code name state
  if ! compat=$(sh_ "$s" "tr '\\0' ' ' </proc/device-tree/compatible"); then
    echo "  $s: adb not usable (unauthorized or offline)"
    [ "$ours" = 1 ] && adb disconnect "$s" >/dev/null 2>&1
    return 0
  fi
  code=$(grep -o 'yandex,[a-z0-9_]*' <<<"$compat" | head -1 | cut -d, -f2 || true)
  if [ -z "$code" ]; then
    echo "  $s: not a Yandex Station"
    [ "$ours" = 1 ] && adb disconnect "$s" >/dev/null 2>&1
    return 0
  fi
  name="${MODEL_NAME[$code]:-unknown Yandex device} ($code)"
  if [ -z "${ARCH_OF[$code]:-}" ]; then
    echo "  $s: $name — not supported yet, skipped"
    [ "$ours" = 1 ] && adb disconnect "$s" >/dev/null 2>&1
    return 0
  fi
  if [ "$(sh_ "$s" 'id -u')" != 0 ]; then
    echo "  $s: $name — adb is not root, skipped"
    [ "$ours" = 1 ] && adb disconnect "$s" >/dev/null 2>&1
    return 0
  fi
  if ! sh_ "$s" "grep -q rootkeeper.sh /system/vendor/quasar/onboot.sh"; then
    echo "  $s: $name — firmware does not run $RK at boot, skipped"
    [ "$ours" = 1 ] && adb disconnect "$s" >/dev/null 2>&1
    return 0
  fi
  if sh_ "$s" "pidof pulsar >/dev/null"; then state="satellite running"
  elif sh_ "$s" "[ -x $D/pulsar ]"; then state="satellite installed, not running"
  else state="stock Yandex"; fi
  FOUND+=("$s")
  ARCH_FOR[$s]=${ARCH_OF[$code]}
  echo "  [${#FOUND[@]}] $s: $name — $state"
}

# errexit does not apply inside a function called from `||`: every step checks its own status.
install_one() { # serial
  local s=$1 want got have hooks tmp f changed=1 sup_changed=1 sup_have action mark
  local bin=$SAT/build/${ARCH_FOR[$s]}/pulsar ssh_out=$ROOT/ssh/out/${ARCH_FOR[$s]}
  [ -s "$bin" ] || { echo "$s: $bin missing" >&2; return 1; }
  echo "== $s: daemon (${ARCH_FOR[$s]})"
  sh_ "$s" "mkdir -p $D" || return 1
  want=$(sha256sum "$bin" | cut -d' ' -f1)
  have=$(sh_ "$s" "[ -f $D/pulsar ] && sha256sum $D/pulsar" | cut -d' ' -f1 || true)
  if [ "$want" = "$have" ]; then
    echo "binary identical to the installed one, not copied (.prev kept)"
    changed=0
  else
    adb -s "$s" push "$bin" "$D/pulsar.new" >/dev/null || return 1
    got=$(sh_ "$s" "sha256sum $D/pulsar.new" | cut -d' ' -f1)
    if [ "$want" != "$got" ]; then
      sh_ "$s" "rm -f $D/pulsar.new" || true
      echo "$s: push corrupted (sha256 mismatch), nothing changed" >&2
      return 1
    fi
  fi
  # Scripts: a running shell reads its script incrementally, so replace them atomically (new inode).
  # A running supervisor keeps executing the old inode: when supervise.sh changed it is reloaded below.
  sup_have=$(sh_ "$s" "[ -f $D/supervise.sh ] && sha256sum $D/supervise.sh" | cut -d' ' -f1 || true)
  [ "$sup_have" != "$(sha256sum "$SAT/deploy/supervise.sh" | cut -d' ' -f1)" ] || sup_changed=0
  for f in boot.sh supervise.sh ctl.sh; do adb -s "$s" push "$SAT/deploy/$f" "$D/$f.new" >/dev/null || return 1; done
  sh_ "$s" "cd $D && chmod 755 boot.sh.new supervise.sh.new ctl.sh.new &&
     mv boot.sh.new boot.sh && mv supervise.sh.new supervise.sh && mv ctl.sh.new ctl.sh" || return 1
  if [ $changed = 1 ]; then
    sh_ "$s" "cd $D && chmod 755 pulsar.new &&
       { [ ! -f pulsar ] || cp -p pulsar pulsar.prev; } &&
       mv pulsar.new pulsar && sync" || return 1
  else
    sh_ "$s" "sync" || true
  fi

  if [ $SSH = 1 ]; then
    echo "== $s: dropbear"
    sh_ "$s" "mkdir -p /data/dropbear" || return 1
    adb -s "$s" push "$ssh_out/dropbearmulti" /data/dropbear/dropbearmulti.new >/dev/null || return 1
    adb -s "$s" push "$ssh_out/sftp-server" /data/dropbear/sftp-server.new >/dev/null || return 1
    adb -s "$s" push "$ROOT/ssh/start.sh" /data/dropbear/start.sh >/dev/null || return 1
    # Running binaries stay busy; mv replaces them atomically.
    sh_ "$s" "cd /data/dropbear && chmod 755 dropbearmulti.new sftp-server.new start.sh &&
       mv dropbearmulti.new dropbearmulti && mv sftp-server.new sftp-server && sync" || return 1
  fi

  echo "== $s: boot hooks"
  tmp=$(mktemp -d)
  if sh_ "$s" "[ -f $RK ]"; then
    adb -s "$s" pull "$RK" "$tmp/rk" >/dev/null || { rm -rf "$tmp"; return 1; }
  else
    printf '#!/bin/sh\n' >"$tmp/rk"
  fi
  hooks=
  grep -qF "/data/pulsar/boot.sh" "$tmp/rk" || hooks=$HOOK_SAT
  if [ $SSH = 1 ] && ! grep -qF "/data/dropbear/start.sh" "$tmp/rk"; then
    hooks=$(printf '%s\n%s' "$HOOK_SSH" "$hooks")
  fi
  if [ -n "$hooks" ]; then
    # Insert before the first top-level endless loop (code after it never runs), else append.
    printf '%s\n' "$hooks" | sed '/^$/d' >"$tmp/hooks"
    awk -v hooks="$tmp/hooks" '
      !done && /^while[ \t]/ { while ((getline l < hooks) > 0) print l; print ""; done = 1 }
      { print }
      END { if (!done) { print ""; while ((getline l < hooks) > 0) print l } }' "$tmp/rk" >"$tmp/rk.new"
    sh -n "$tmp/rk.new" || { rm -rf "$tmp"; return 1; }
    adb -s "$s" push "$tmp/rk.new" "$RK.new" >/dev/null || { rm -rf "$tmp"; return 1; }
    sh_ "$s" "{ [ ! -f $RK ] || cp -p $RK $RK.bak-\$(date +%Y%m%d%H%M%S); } && chmod 755 $RK.new && mv $RK.new $RK && sync" \
      || { rm -rf "$tmp"; return 1; }
    echo "added to $RK:"
    sed 's/^/  /' "$tmp/hooks"
  else
    echo "already present in $RK"
  fi
  rm -rf "$tmp"

  if [ $SSH = 1 ]; then
    sh_ "$s" "pidof dropbear >/dev/null || /data/dropbear/start.sh </dev/null >/dev/null 2>&1 &"
  fi
  if [ $ENABLE = 1 ]; then
    echo "== $s: start"
    # Marker in the log: the health check looks only at what happens after it.
    mark="=== install-marker $$ $(date +%s)"
    sh_ "$s" "echo '$mark' >> /tmp/pulsar.log" || true
    action=restart
    [ $sup_changed = 0 ] || action=reload
    sh_ "$s" "if [ -f $D/enabled ] && { pidof supervise.sh || pidof pulsar; } >/dev/null; then $D/ctl.sh $action; else $D/ctl.sh enable; fi"
    echo
    health_check "$s" "$changed" "$mark" || return 1
    sh_ "$s" "$D/ctl.sh status; $D/ctl.sh log 5"
    echo
  fi
}

# After a (re)start, at ~10 s and again at ~30 s: the process is alive and, in the log after the
# install marker, there is exactly one `=== start` and no crash report, exit or supervisor message
# (a binary that crashes and is restarted in between would otherwise look healthy). On failure,
# offer to put the previous binary back.
health_check() { # serial binary_changed marker
  local s=$1 changed=$2 mark=$3 log full ans n bad=0 unknown=0 round
  for round in 1 2; do
    if [ $round = 1 ]; then sleep 10; else sleep 20; fi
    # The log may have been copy-truncated by supervise.sh meanwhile: the older half is in .1.
    full=$(sh_ "$s" "cat /tmp/pulsar.log.1 /tmp/pulsar.log 2>/dev/null | tail -n 1500" || true)
    log=$(sh_ "$s" "tail -n 600 /tmp/pulsar.log" || true)
    if [[ $log == *"$mark"* ]]; then :; elif [[ $full == *"$mark"* ]]; then log=$full; else log=; fi
    if [ -z "$log" ]; then
      log="(install marker not found in the log, even in .1)"
      unknown=1
      break
    fi
    # The old daemon's exit (and the restart) land after the marker: judge only what follows the
    # first `=== start` after it, which is the new daemon's.
    log=${log#*"$mark"}
    if [[ $log != *"=== start"* ]]; then bad=1; log="(no start after the install marker)$log"; break; fi
    log="=== start${log#*"=== start"}"
    n=$(grep -c '^=== start' <<<"$log" || true)
    if ! sh_ "$s" "pidof pulsar >/dev/null" || [ "$n" != 1 ] \
      || [[ $log == *"=== CRASH"* || $log == *"=== exit"* || $log == *"=== supervise"* ]]; then bad=1; break; fi
  done
  if [ $unknown = 1 ]; then
    echo "$s: health check UNKNOWN: $log; not rolling back automatically" >&2
    return 1
  fi
  if [ $bad = 0 ]; then
    echo "$s: health check ok"
    return 0
  fi
  echo "$s: health check FAILED, log:" >&2
  printf '%s\n' "$log" | tail -n 15 >&2
  if [ "$changed" = 1 ] && sh_ "$s" "[ -f $D/pulsar.prev ]"; then
    ans=y
    [ $YES = 1 ] || read -r -p "roll back to the previous binary? [Y/n] " ans </dev/tty
    if [[ ${ans:-y} != [nN]* ]]; then
      sh_ "$s" "cd $D && cp -p pulsar.prev pulsar.rb && mv pulsar pulsar.bad &&
        mv pulsar.rb pulsar && sync && ./ctl.sh restart" || true
      echo "$s: rolled back (bad binary kept as $D/pulsar.bad)" >&2
    fi
  fi
  return 1
}

command -v adb >/dev/null || { echo "adb not found (Android platform-tools)" >&2; exit 1; }
[ $LIST = 1 ] || build

echo "== looking for stations"
declare -A seen=()
if [ ${#TARGETS[@]} -eq 0 ]; then
  # Already connected network devices first, then mDNS (or the port scan).
  mapfile -t connected < <(adb devices | awk 'NR > 1 && $2 == "device" && $1 ~ /:[0-9]+$/ {print $1}')
  for s in "${connected[@]}"; do seen[$s]=1; probe "$s" 0; done
  if [ $SCAN = 0 ] && command -v avahi-browse >/dev/null; then
    mapfile -t TARGETS < <(mdns)
  else
    mapfile -t TARGETS < <(scan)
  fi
fi
for s in "${TARGETS[@]}"; do
  [ -z "${seen[$s]:-}" ] || continue
  seen[$s]=1
  ours=0
  adb devices | grep -q "^$s[[:space:]]*device" || { adb connect "$s" >/dev/null 2>&1 || true; ours=1; }
  probe "$s" "$ours"
done
[ ${#FOUND[@]} -gt 0 ] || { echo "no supported rooted station found"; exit 1; }
[ $LIST = 0 ] || exit 0

chosen=()
if [ $YES = 1 ]; then
  chosen=("${FOUND[@]}")
else
  read -r -p "install to which? (numbers separated by spaces, a = all, Enter = cancel): " ans </dev/tty
  if [ "$ans" = a ]; then chosen=("${FOUND[@]}")
  else
    for n in $ans; do
      [[ $n =~ ^[0-9]+$ ]] && [ "$n" -ge 1 ] && [ "$n" -le ${#FOUND[@]} ] || { echo "bad choice: $n" >&2; exit 1; }
      chosen+=("${FOUND[$((n - 1))]}")
    done
  fi
fi
[ ${#chosen[@]} -gt 0 ] || { echo "nothing to do"; exit 0; }

failed=0
for s in "${chosen[@]}"; do install_one "$s" || failed=1; done
exit $failed
