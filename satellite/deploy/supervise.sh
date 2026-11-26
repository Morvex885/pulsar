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
#
# Stale ALSA dmix lock: the firmware's alsa-lib (patched by Yandex, Midi and Lite 2) does not start
# the dmix slave PCM on open; the first client to start does, and the dmix shm segment (ipc_key
# 1024 = 0x400 from the firmware's asound config, 0x401 = its mix buffer, semaphore 0x400) counts
# the running clients. A client that dies while playing (crash, SIGKILL, a test's kill -SEGV) never
# decrements the count, and nobody removes the segment: every later open reuses it, the slave stays
# PREPARED (never triggered) and writers block forever on the dmix timer, i.e. silence until a
# reboot. Before each launch (no pulsar running) segments nobody is attached to are removed.
#
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

# Старая ctl.sh не гарантирует возврат player при disable или откате.
bt_allowed=${PULSAR_BT_MANAGED:-1}
bt_owner=$("$D/ctl.sh" bt-restore --owner-path 2>/dev/null) || bt_owner=
bt_legacy=/tmp/pulsar-bt-owner
bt_blocked=$BIN.bt-blocked
bt_next=$bt_blocked.next
# Каталог /data/pulsar доверенный, как и ctl.sh. Запускаем закреплённый на эту
# загрузку inode; имя pulsar нужно для pidof.
boot_id=$(cat /proc/sys/kernel/random/boot_id 2>/dev/null)
bt_runtime= bt_executable=
case "$boot_id" in
  ''|*[!0-9a-f-]*) boot_id= ;;
  *) bt_runtime=$D/.pulsar-bt-executable.$boot_id; bt_executable=$bt_runtime/pulsar ;;
esac

bt_blocked_now() { [ "$BIN" -ef "$bt_blocked" ] || [ "$BIN" -ef "$bt_next" ]; }
bt_pinned_now() { [ ! -L "$bt_executable" ] && [ "$BIN" -ef "$bt_executable" ]; }
bt_pending() { [ -e "$bt_owner" ] || [ -L "$bt_owner" ]; }

bt_test_clear() {
  # Проверяем здесь, даже если установленная ctl.sh ещё не знает о тестовом снимке.
  bt_test_dir=/tmp/pulsar-test
  [ -d "$bt_test_dir" ] && [ ! -L "$bt_test_dir" ] &&
    { [ -e "$bt_test_dir/bt-owner" ] || [ -L "$bt_test_dir/bt-owner" ]; } &&
    bt_test_uid=$(id -u) &&
    [ "$(stat -c '%u:%a' "$bt_test_dir" 2>/dev/null)" = "$bt_test_uid:700" ] || return 0
  say 'Bluetooth тестового экземпляра не восстановлен: выполните test-stop'
  return 1
}

remember_bt_executable() {
  [ -n "$bt_executable" ] && [ -n "$bt_owner" ] || return 1
  bt_pinned_now && return 0
  # При замене бинаря не перепривязываем снимок к чужому inode.
  bt_pending && return 1
  if [ -z "${PULSAR_BT_OWNER:-}" ]; then
    # Новый boot_id не должен применять флаги от предыдущей загрузки.
    for old in "$D"/.pulsar-bt-owner.*; do
      [ "$old" = "$bt_owner" ] || rm -f "$old" || return 1
    done
  fi
  for old in "$D"/.pulsar-bt-executable.*; do
    if [ -d "$old" ]; then
      rm -f "$old/pulsar" && rmdir "$old" || return 1
    else
      rm -f "$old" || return 1
    fi
  done
  mkdir "$bt_runtime" && ln "$BIN" "$bt_executable"
}

restore_bt() {
  restore_owner=${1:-$bt_owner}
  if [ -e "$restore_owner" ] || [ -L "$restore_owner" ]; then
    if ! PULSAR_BT_OWNER=$restore_owner "$D/ctl.sh" bt-restore >> "$LOG" 2>&1; then
      say "Bluetooth restore failed; not starting another binary or the Yandex stack"
      return 1
    fi
  fi
  return 0
}

# Не запускать ни один стек, пока владение не возвращено. При disable решение
# о запуске штатного стека принимает ctl.sh, а не ожидающий supervisor.
wait_restore_bt() {
  restore_delay=$BACKOFF_MIN
  while [ -f "$D/enabled" ]; do
    bt_test_clear || return 1
    if restore_bt "$@"; then
      [ -f "$D/enabled" ]
      return
    fi
    sleep "$restore_delay"
    restore_delay=$((restore_delay * 2))
    [ "$restore_delay" -le "$BACKOFF_MAX" ] || restore_delay=$BACKOFF_MAX
  done
  return 1
}

install_prev() {
  # Запрет на захват для текущего бинаря действует до замены рабочего пути.
  if [ "$BIN" -ef "$bt_next" ]; then mv -f "$bt_next" "$bt_blocked" || return 1; fi
  rm -f "$BIN.tmp" "$bt_next" && cp -p "$BIN.prev" "$BIN.tmp" &&
    ln "$BIN.tmp" "$bt_next" && mv -f "$BIN.tmp" "$BIN" ||
    { rm -f "$BIN.tmp" "$bt_next"; return 1; }
}

give_back() { # Возврат стека Яндекса, как в ctl.sh disable.
  wait_restore_bt || exit 0
  bt_test_clear || exit 1
  rm -f "$D/enabled"
  say "$1; disabled, starting the Yandex stack"
  bt_test_clear || exit 1
  pidof quasar_launcher2 >/dev/null 2>&1 || "$Q" start >/dev/null 2>&1
  exit 1
}

clear_dmix() {
  segs=$(ipcs -m 2>/dev/null) || return 0
  removed= busy=0
  # 0x400/0x401: the firmware's speakers dmix; 0x8e5/0x8e6: the Mini's minidmix.
  for k in 400 401 8e5 8e6; do
    n=$(echo "$segs" | awk -v k="0x00000$k" '$1 == k { print $6; exit }')
    if [ "$n" = 0 ]; then
      ipcrm -M "0x$k" && removed="$removed shm 0x$k"
    elif [ -n "$n" ]; then
      busy=1
    fi
  done
  for k in 400 8e5; do
    if [ $busy = 0 ] && ipcs -s 2>/dev/null | awk -v k="0x00000$k" '$1 == k { f = 1 } END { exit !f }'; then
      ipcrm -S "0x$k" && removed="$removed sem 0x$k"
    fi
  done
  [ -z "$removed" ] || echo "=== alsa: removed stale dmix lock (${removed# }) $(date)" >> "$LOG"
}

( while sleep 60; do rotate; done ) &
ROT=$!
# busybox ash does not run an EXIT trap on SIGTERM: clean up in the signal traps themselves.
trap 'kill $ROT 2>/dev/null; exit 143' TERM INT HUP
trap 'kill $ROT 2>/dev/null' EXIT

fails=0 swapped=0 backoff=$BACKOFF_MIN last_inode=
bt_test_clear || exit 1
if [ -z "${PULSAR_BT_OWNER:-}" ] &&
  [ "$(stat -c '%u' "$bt_legacy" 2>/dev/null)" = "$(id -u)" ]; then
  # Старый снимок не связан с новым путём и не должен пережить новый захват.
  wait_restore_bt "$bt_legacy" || exit 0
fi
if [ -z "$bt_owner" ] && [ -n "$boot_id" ]; then
  # Если ctl заменили старой версией, снимок всё равно нельзя оставлять новому плееру.
  wait_restore_bt "$D/.pulsar-bt-owner.$boot_id" || exit 0
fi
while [ -f "$D/enabled" ]; do
  bt_test_clear || exit 1
  rotate
  if [ ! -x "$BIN" ]; then
    say "$BIN missing or not executable"
    if [ "$swapped" = 0 ] && [ -x "$BIN.prev" ]; then
      wait_restore_bt || exit 0
      [ ! -x "$BIN" ] || continue
      install_prev && swapped=1 && fails=0 && say "restored .prev" && continue
    fi
    give_back "no usable binary"
  fi
  inode=$(stat -Lc '%d:%i' "$BIN") || { say "could not identify $BIN"; exit 1; }
  clear_dmix
  bt_managed=0 launch=$BIN installed_binary=
  if bt_blocked_now; then
    wait_restore_bt || exit 0
    if [ "$BIN" -ef "$bt_next" ]; then mv -f "$bt_next" "$bt_blocked"; fi
  else
    rm -f "$bt_blocked" "$bt_next"
    if [ "$bt_allowed" = 1 ] && "$D/ctl.sh" bt-restore --check >/dev/null 2>&1; then
      if bt_pinned_now; then
        bt_managed=1
      else
        wait_restore_bt || exit 0
        remember_bt_executable && bt_pinned_now && bt_managed=1
      fi
      if [ "$bt_managed" = 1 ] && [ -z "${PULSAR_BT_OWNER:-}" ]; then
        # Рабочий путь могут заменить после последней сверки inode, даже при
        # первом запуске. Новый бинарь пойдёт только после возврата владения.
        launch=$bt_executable installed_binary=$BIN
      fi
    else
      wait_restore_bt || exit 0
    fi
    # Появился другой бинарь — он получает собственную попытку отката.
    if [ "$swapped" = 1 ]; then swapped=0 fails=0 backoff=$BACKOFF_MIN; fi
  fi
  [ "$inode" = "$(stat -Lc '%d:%i' "$BIN")" ] || continue
  if [ "$bt_managed" = 1 ] && ! bt_pinned_now; then continue; fi
  last_inode=$inode
  start_at=$(date)
  t0=$(uptime_s)
  bt_test_clear || exit 1
  echo "=== start $start_at" >> "$LOG"
  PULSAR_BT_MANAGED=$bt_managed PULSAR_BT_OWNER=$bt_owner PULSAR_INSTALLED_BINARY=$installed_binary \
    "$launch" "$D/state.conf" >> "$LOG" 2>&1
  rc=$?
  up=$(($(uptime_s) - t0))
  echo "=== exit $rc after ${up}s $(date)" >> "$LOG"
  if bt_pending; then
    "$D/ctl.sh" bt-restore --hide >> "$LOG" 2>&1 ||
      say "Bluetooth pairing flags could not be closed after daemon exit"
  fi

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
      wait_restore_bt || exit 0
      [ "$last_inode" = "$(stat -Lc '%d:%i' "$BIN")" ] || continue
      if { [ "$BIN" -ef "$BIN.bad" ] || ln -f "$BIN" "$BIN.bad"; } && install_prev; then
        swapped=1 fails=0 backoff=$BACKOFF_MIN
        note=; [ -f "$D/api_key" ] && note="; api_key is set: the previous binary may not support API encryption, so Home Assistant (which holds the key) may be unable to connect - remove the key in HA or re-add the device"
        say "$CRASHES short crashes in a row, rolled back to .prev (bad binary kept as .bad)$note"
        continue
      fi
    fi
    if [ "$swapped" = 1 ]; then give_back "$CRASHES short crashes in a row, also with the rolled-back .prev"
    else give_back "$CRASHES short crashes in a row, no different previous binary to fall back to"; fi
  fi
  sleep $backoff
  backoff=$((backoff * 2))
  [ $backoff -gt $BACKOFF_MAX ] && backoff=$BACKOFF_MAX
done
exit 0
