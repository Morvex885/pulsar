#!/usr/bin/env bash
# Finds rooted Yandex Stations on the local network and installs or updates pulsar.
#
#   satellite/deploy/install.sh [--list] [--scan] [--ssh] [--no-enable] [--yes] [ip[:port] ...]
#
# Without addresses: finds Yandex Stations via mDNS (avahi-browse) or, with --scan or without
# avahi, by scanning the /24 of every default-route interface for adb (port 5555); connects,
# identifies the model and offers the supported, rooted stations found. Per station:
# - migrates an install from before the rename (/data/cucumber-satellite): removes its `enabled`,
#   stops its supervisor and daemon (the Yandex stack stays stopped), copies state.conf, api_key
#   and sendspin/ to /data/pulsar (files already there win), keeps the old binary as pulsar.prev
#   and replaces its boot hook in place by the pulsar one; once pulsar passes the health check, the
#   old directory is renamed to /data/cucumber-satellite.migrated-<timestamp>. On any failure
#   before that, the old hook, `enabled` and supervisor are restored (with --no-enable too, a
#   running old install is restarted as pulsar);
# - copies the daemon + scripts to /data/pulsar (scripts via .new + mv; when the binary
#   differs from the installed one, the installed one is kept as .prev; an identical one is not
#   copied and leaves .prev alone);
# - --ssh also installs dropbear (static build from ssh/build.sh) to /data/dropbear;
# - adds the boot hooks to /data/quasar/rootkeeper.sh (backup: rootkeeper.sh.bak-<timestamp>),
#   placed before an endless `while` loop if the script has one, otherwise appended, and shows the
#   diff against the backup;
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
# Installs from before the rename to pulsar; migrated by install_one.
OLD=/data/cucumber-satellite
HOOK_OLD='[ -x /data/cucumber-satellite/boot.sh ] && /data/cucumber-satellite/boot.sh </dev/null >/dev/null 2>&1 &'

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
    -h|--help) awk 'NR > 1 { if (!/^#/) exit; print substr($0, 3) }' "$0"; exit 0 ;;
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
  if sh_ "$s" "[ -d $OLD ]"; then state="installed under the old name cucumber-satellite, will be migrated to $D"
  elif sh_ "$s" "pidof pulsar >/dev/null"; then state="satellite running"
  elif sh_ "$s" "[ -x $D/pulsar ]"; then state="satellite installed, not running"
  else state="stock Yandex"; fi
  FOUND+=("$s")
  ARCH_FOR[$s]=${ARCH_OF[$code]}
  echo "  [${#FOUND[@]}] $s: $name — $state"
}

# Device-side migration steps, prepended to each migration command. Supervisors are found by argv
# (`sh <dir>/supervise.sh`, which also matches their log-rotation subshell), not by name: both the
# old and the pulsar one are called supervise.sh. Never `ctl.sh disable`: it starts the Yandex stack.
# SIGHUP is ignored so that a dropped adb session cannot cut a step short. Sent without indentation
# and comment lines: an adb shell command is limited to 4 KB on these firmwares.
MIG_SH="O=$OLD D=$D
$(sed -e 's/^ *//' -e '/^#/d' <<'EOF'
trap '' HUP
sups() { # dir
  d=$1
  for p in /proc/[0-9]*; do
    set -- $(tr '\0' ' ' 2>/dev/null <"$p/cmdline")
    [ $# = 2 ] && [ "$2" = "$d/supervise.sh" ] && echo "${p#/proc/}"
  done
  return 0
}
stopd() { # name: SIGTERM, up to 10 s, then SIGKILL
  pidof "$1" >/dev/null || return 0
  killall "$1"
  i=0; while [ $i -lt 10 ] && pidof "$1" >/dev/null; do sleep 1; i=$((i + 1)); done
  pidof "$1" >/dev/null || return 0
  echo "$1 did not exit in 10 s, killing it"
  killall -9 "$1"
  i=0; while [ $i -lt 5 ] && pidof "$1" >/dev/null; do sleep 1; i=$((i + 1)); done
}
stop_all() { # dir daemon
  rm -f "$1/enabled" || return 1
  # Supervisors first: on SIGTERM they quit once their child exits instead of restarting it.
  kill $(sups "$1") 2>/dev/null
  stopd "$2"
  i=0; while [ $i -lt 5 ] && [ -n "$(sups "$1")" ]; do sleep 1; i=$((i + 1)); done
  [ -z "$(sups "$1")" ] || { kill -9 $(sups "$1") 2>/dev/null; sleep 1; }
  stopd "$2"
  [ -z "$(sups "$1")" ] && ! pidof "$2" >/dev/null
}
mig_state() {
  [ ! -f "$O/enabled" ] || echo "old install enabled"
  [ -z "$(sups "$O")" ] && ! pidof cucumber-satellite >/dev/null || echo "old install running"
}
# Copies named in $D/.migrating belong to the migration: refreshed by a re-run, removed by
# mig_undo. Anything else already in $D wins.
carry() { # old_name new_name
  [ -e "$O/$1" ] || return 0
  if [ -e "$D/$2" ] && ! grep -qx "$2" "$D/.migrating" 2>/dev/null; then
    echo "kept $D/$2 (already there), $O/$1 not copied"
    return 0
  fi
  grep -qx "$2" "$D/.migrating" 2>/dev/null || echo "$2" >>"$D/.migrating" || return 1
  rm -rf "$D/$2.mig" && cp -Rp "$O/$1" "$D/$2.mig" && rm -rf "$D/$2" && mv "$D/$2.mig" "$D/$2" &&
    echo "copied $O/$1 to $D/$2" && return 0
  rm -rf "$D/$2.mig"
  return 1
}
mig_copy() { # old_enabled
  [ -d "$D" ] || { mkdir "$D" && echo "created $D"; } || return 1
  carry state.conf state.conf && carry api_key api_key && carry sendspin sendspin || return 1
  [ ! -f "$D/api_key" ] || chmod 600 "$D/api_key" || return 1
  # Crash-loop rollback target: the old daemon takes every path from its state file argument.
  [ -e "$D/pulsar" ] || carry cucumber-satellite pulsar.prev || return 1
  [ "$1" = 0 ] || touch "$D/enabled" || return 1
  sync
}
mig_undo() { # ts old_was_running old_enabled dir_created
  stop_all "$D" pulsar || { echo "pulsar did not stop, old install not restarted"; return 1; }
  if [ "$4" = 1 ]; then
    [ ! -e "$D" ] || { mv "$D" "$D.failed-$1" && echo "moved $D to $D.failed-$1"; } || return 1
  elif [ -f "$D/.migrating" ]; then
    for f in $(cat "$D/.migrating"); do rm -rf "$D/$f"; done
    rm -f "$D/.migrating" && echo "removed the copies from $D"
  fi
  [ "$3" = 0 ] || { touch "$O/enabled" && echo "recreated $O/enabled"; } || return 1
  sync
  if [ -n "$(sups "$O")" ] || pidof cucumber-satellite >/dev/null; then
    echo "old install still running"
    return 0
  fi
  if [ "$2" = 1 ]; then
    (exec setsid "$O/supervise.sh" </dev/null >/dev/null 2>&1) &
    i=0; while [ $i -lt 3 ] && [ -z "$(sups "$O")" ]; do sleep 1; i=$((i + 1)); done
    [ -z "$(sups "$O")" ] || { echo "old supervisor restarted"; return 0; }
    echo "old supervisor did not start"
  fi
  pidof quasar_launcher2 >/dev/null && return 0
  /system/vendor/quasar/q start >/dev/null 2>&1
  echo "started the Yandex stack"
}
mig_done() { # ts
  rm -f "$D/.migrating" && sync && mv "$O" "$O.migrated-$1" && sync && echo "renamed $O to $O.migrated-$1"
}
EOF
)"

# Stops an install from before the rename and copies its state into $D. The old directory stays
# as it is (minus `enabled`) until pulsar passed the health check (finish_migration). Sets
# MIG_RUNNING / MIG_ENABLED / MIG_NEWDIR for unmigrate, which runs here on failure.
migrate() { # serial ts
  local s=$1 ts=$2 out rc=0
  MIG_RUNNING=0 MIG_ENABLED=0 MIG_NEWDIR=0
  echo "== $s: migrating $OLD (installed before the rename) to $D"
  out=$(sh_ "$s" "$MIG_SH
mig_state") || { echo "$s: cannot read the state of $OLD, nothing changed" >&2; return 1; }
  [[ $out != *"old install enabled"* ]] || MIG_ENABLED=1
  [[ $out != *"old install running"* ]] || MIG_RUNNING=1
  [ $MIG_RUNNING = 0 ] || echo "stopping the old supervisor and cucumber-satellite (the Yandex stack stays stopped)"
  [ $MIG_ENABLED = 0 ] || echo "removing $OLD/enabled"
  out=$(sh_ "$s" "$MIG_SH
stop_all $OLD cucumber-satellite") || rc=1
  [ -z "$out" ] || printf '%s\n' "$out"
  if [ $rc = 0 ]; then
    out=$(sh_ "$s" "$MIG_SH
mig_copy $MIG_ENABLED") || rc=1
    [ -z "$out" ] || printf '%s\n' "$out"
    [[ $out != *"created $D"* ]] || MIG_NEWDIR=1
  fi
  [ $rc = 0 ] || { echo "$s: migration failed" >&2; unmigrate "$s" "$ts"; return 1; }
}

# Puts the install from before the rename back: rootkeeper.sh from this run's backup (the old hook
# line), pulsar stopped, the copies removed from $D (or $D moved aside to $D.failed-<ts> if this
# run created it), the old `enabled` recreated and the old supervisor restarted if it was running
# (otherwise the Yandex stack is started if it is not running).
unmigrate() { # serial ts
  local s=$1 ts=$2 out rc=0
  echo "== $s: restoring $OLD" >&2
  out=$(sh_ "$s" "$MIG_SH
r=0
if [ -f $RK.bak-$ts ]; then
  cp -p $RK.bak-$ts $RK.new && mv $RK.new $RK && sync && echo 'restored $RK from $RK.bak-$ts' || r=1
fi
mig_undo $ts $MIG_RUNNING $MIG_ENABLED $MIG_NEWDIR || r=1
[ \$r = 0 ]") || rc=1
  [ -z "$out" ] || printf '%s\n' "$out" >&2
  [ $rc = 0 ] || echo "$s: RESTORE INCOMPLETE: check $RK (backup $RK.bak-$ts), $OLD/enabled and that exactly one supervisor runs" >&2
  return $rc
}

# After pulsar passed the health check: the old directory becomes the backup $OLD.migrated-<ts>.
finish_migration() { # serial ts
  local out
  if out=$(sh_ "$1" "$MIG_SH
mig_done $2"); then
    printf '%s\n' "$out"
    echo "$1: migrated to $D; the old install is kept as $OLD.migrated-$2"
  else
    [ -z "$out" ] || printf '%s\n' "$out" >&2
    echo "$1: pulsar runs, but $OLD could not be renamed to $OLD.migrated-$2 (re-run to finish)" >&2
    return 1
  fi
}

# errexit does not apply inside a function called from `||`: every step checks its own status.
install_one() { # serial
  local s=$1 ts mark action changed=1 sup_changed=1 mig=0
  local bin=$SAT/build/${ARCH_FOR[$s]}/pulsar
  [ -s "$bin" ] || { echo "$s: $bin missing" >&2; return 1; }
  ts=$(date +%Y%m%d%H%M%S)
  MIG_RUNNING=0
  if sh_ "$s" "[ -d $OLD ]"; then
    migrate "$s" "$ts" || return 1
    mig=1
  fi
  # deploy_files sets changed / sup_changed (bash locals are visible to the functions called).
  if ! deploy_files "$s" "$bin" "$ts"; then
    [ $mig = 0 ] || unmigrate "$s" "$ts"
    return 1
  fi
  # A migrated install that was running keeps running (as pulsar), --no-enable or not.
  if [ $ENABLE = 1 ] || [ $MIG_RUNNING = 1 ]; then
    echo "== $s: start"
    # Marker in the log: the health check looks only at what happens after it.
    mark="=== install-marker $$ $(date +%s)"
    sh_ "$s" "echo '$mark' >> /tmp/pulsar.log" || true
    action=restart
    [ $sup_changed = 0 ] || action=reload
    sh_ "$s" "if [ -f $D/enabled ] && { pidof supervise.sh || pidof pulsar; } >/dev/null; then $D/ctl.sh $action; else $D/ctl.sh enable; fi"
    echo
    # A failed migration goes back to the old install, not to pulsar.prev.
    if ! health_check "$s" "$((changed && !mig))" "$mark"; then
      [ $mig = 0 ] || unmigrate "$s" "$ts"
      return 1
    fi
    sh_ "$s" "$D/ctl.sh status; $D/ctl.sh log 5"
    echo
  fi
  [ $mig = 0 ] || finish_migration "$s" "$ts"
}

# Daemon, scripts, dropbear (--ssh) and boot hooks. Sets changed / sup_changed of install_one.
deploy_files() { # serial binary ts
  local s=$1 bin=$2 ts=$3 want got have hooks tmp f sup_have swap nold bad
  local ssh_out=$ROOT/ssh/out/${ARCH_FOR[$s]}
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
  # The hook from before the rename gives way to the pulsar one: replaced in place when it comes
  # before the endless loop (where install.sh put it) and there is no pulsar hook yet, else removed.
  swap=0 nold=$(grep -cxF "$HOOK_OLD" "$tmp/rk" || true)
  if [ "$nold" -gt 0 ] && ! grep -qF "/data/pulsar/boot.sh" "$tmp/rk" &&
    [ -n "$(awk -v old="$HOOK_OLD" '/^while[ \t]/ { exit } $0 == old { print; exit }' "$tmp/rk")" ]; then
    swap=1
  fi
  hooks=
  [ $swap = 1 ] || grep -qF "/data/pulsar/boot.sh" "$tmp/rk" || hooks=$HOOK_SAT
  if [ $SSH = 1 ] && ! grep -qF "/data/dropbear/start.sh" "$tmp/rk"; then
    hooks=$(printf '%s\n%s' "$HOOK_SSH" "$hooks")
  fi
  if [ -n "$hooks" ] || [ "$nold" -gt 0 ]; then
    # Insert before the first top-level endless loop (code after it never runs), else append.
    printf '%s\n' "$hooks" | sed '/^$/d' >"$tmp/hooks"
    awk -v hooks="$tmp/hooks" -v old="$HOOK_OLD" -v new="$HOOK_SAT" -v swap=$swap '
      !done && /^while[ \t]/ { while ((getline l < hooks) > 0) { print l; n++ } if (n) print ""; done = 1 }
      $0 == old { if (swap) print new; swap = 0; next }
      { print }
      END { if (!done) while ((getline l < hooks) > 0) { if (!n++) print ""; print l } }' "$tmp/rk" >"$tmp/rk.new"
    # Nothing but our hook lines and blank separators may change.
    bad=$(diff "$tmp/rk" "$tmp/rk.new" | grep '^[<>]' |
      grep -vxF -e "< $HOOK_OLD" -e "> $HOOK_SAT" -e "> $HOOK_SSH" -e '>' -e '> ' || true)
    if [ -n "$bad" ] || ! sh -n "$tmp/rk.new"; then
      echo "$s: unexpected change to $RK, not written:" >&2
      printf '%s\n' "$bad" >&2
      rm -rf "$tmp"
      return 1
    fi
    adb -s "$s" push "$tmp/rk.new" "$RK.new" >/dev/null || { rm -rf "$tmp"; return 1; }
    sh_ "$s" "{ [ ! -f $RK ] || cp -p $RK $RK.bak-$ts; } && chmod 755 $RK.new && mv $RK.new $RK && sync" \
      || { rm -rf "$tmp"; return 1; }
    if [ "$(sh_ "$s" "sha256sum $RK" | cut -d' ' -f1)" != "$(sha256sum "$tmp/rk.new" | cut -d' ' -f1)" ]; then
      echo "$s: $RK on the device differs from the checked file" >&2
      rm -rf "$tmp"
      return 1
    fi
    if sh_ "$s" "[ -f $RK.bak-$ts ]"; then
      echo "$RK changed, diff against $RK.bak-$ts on the device:"
      sh_ "$s" "diff $RK.bak-$ts $RK" | sed 's/^/  /' || true
    else
      echo "created $RK with:"
      sed 's/^/  /' "$tmp/hooks"
    fi
  else
    echo "already present in $RK"
  fi
  rm -rf "$tmp"

  if [ $SSH = 1 ]; then
    sh_ "$s" "pidof dropbear >/dev/null || /data/dropbear/start.sh </dev/null >/dev/null 2>&1 &"
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
