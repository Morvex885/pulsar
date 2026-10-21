#!/bin/sh
# Keeps pulsar running while /data/pulsar/enabled exists.
# Log: /tmp/pulsar.log (tmpfs, rotated at 1 MB, one .1 kept).
#
# Exit status of the daemon: 0 = deliberate exit (SIGTERM from ctl.sh restart, the "Restart" button
# in Home Assistant): restart after 1 s, neither counted nor forgiven as a crash unless it ran
# < MINUP seconds. Anything else (non-zero, killed by a signal, e.g. 139 = SIGSEGV) is a crash.
# A "short crash" is one after a run of < LONG seconds. CRASHES consecutive short crashes: swap in
# pulsar.prev (the bad binary is kept as .bad) and try again; if that crash-loops too,
# remove `enabled` and give the speaker back to the Yandex stack (same as `ctl.sh disable`).
# Only a run of >= LONG seconds resets the count. Restart delay after a crash: 3 s, doubling up
# to 30 s.
# PULSAR_DIR, PULSAR_LOG, PULSAR_Q override the device paths (local dry runs).
D=${PULSAR_DIR:-/data/pulsar}
LOG=${PULSAR_LOG:-/tmp/pulsar.log}
Q=${PULSAR_Q:-/system/vendor/quasar/q}
BIN=$D/pulsar
MAXLOG=1000000
LONG=${PULSAR_LONG:-300}
CRASHES=5
MINUP=5
BACKOFF_MIN=3
BACKOFF_MAX=30

# Seconds since boot: unaffected by clock steps (NTP / time from Home Assistant).
uptime_s() { read -r u _ < /proc/uptime; echo "${u%%.*}"; }

# Copy-truncate: the daemon keeps its append-mode descriptor, so a rename would not stop growth.
rotate() {
  if [ -f "$LOG" ] && [ "$(wc -c < "$LOG")" -gt $MAXLOG ]; then
    cp "$LOG" "$LOG.1" && : > "$LOG"
  fi
}

say() { echo "=== supervise: $* $(date)" >> "$LOG"; }

give_back() { # Yandex stack again, like ctl.sh disable
  rm -f "$D/enabled"
  say "$1; disabled, starting the Yandex stack"
  pidof quasar_launcher2 >/dev/null 2>&1 || "$Q" start >/dev/null 2>&1
  exit 1
}

( while sleep 60; do rotate; done ) &
ROT=$!
# busybox ash does not run an EXIT trap on SIGTERM: clean up in the signal traps themselves.
trap 'kill $ROT 2>/dev/null; exit 143' TERM INT HUP
trap 'kill $ROT 2>/dev/null' EXIT

fails=0 swapped=0 backoff=$BACKOFF_MIN
while [ -f "$D/enabled" ]; do
  rotate
  if [ ! -x "$BIN" ]; then
    say "$BIN missing or not executable"
    if [ "$swapped" = 0 ] && [ -x "$BIN.prev" ]; then
      cp -p "$BIN.prev" "$BIN.tmp" && mv "$BIN.tmp" "$BIN" && swapped=1 && fails=0 && say "restored .prev" && continue
    fi
    give_back "no usable binary"
  fi
  echo "=== start $(date)" >> "$LOG"
  t0=$(uptime_s)
  "$BIN" "$D/state.conf" >> "$LOG" 2>&1
  rc=$?
  up=$(($(uptime_s) - t0))
  echo "=== exit $rc after ${up}s $(date)" >> "$LOG"

  if [ $up -ge $LONG ]; then
    fails=0 swapped=0 backoff=$BACKOFF_MIN
    [ $rc -eq 0 ] && sleep 1 || sleep $backoff
    continue
  fi
  if [ $rc -eq 0 ] && [ $up -ge $MINUP ]; then
    sleep 1
    continue
  fi
  fails=$((fails + 1))
  if [ $fails -ge $CRASHES ]; then
    if [ "$swapped" = 0 ] && [ -f "$BIN.prev" ] && ! cmp -s "$BIN" "$BIN.prev"; then
      mv -f "$BIN" "$BIN.bad"
      if cp -p "$BIN.prev" "$BIN.tmp" && mv "$BIN.tmp" "$BIN"; then
        swapped=1 fails=0 backoff=$BACKOFF_MIN
        note=; [ -f "$D/api_key" ] && note="; api_key is set: the previous binary may not support API encryption, so Home Assistant (which holds the key) may be unable to connect - remove the key in HA or re-add the device"
        say "$CRASHES short crashes in a row, rolled back to .prev (bad binary kept as .bad)$note"
        continue
      fi
      mv -f "$BIN.bad" "$BIN"
    fi
    if [ "$swapped" = 1 ]; then give_back "$CRASHES short crashes in a row, also with the rolled-back .prev"
    else give_back "$CRASHES short crashes in a row, no different previous binary to fall back to"; fi
  fi
  sleep $backoff
  backoff=$((backoff * 2))
  [ $backoff -gt $BACKOFF_MAX ] && backoff=$BACKOFF_MAX
done
