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
bt_exists() { [ -e "$1" ] || [ -L "$1" ]; }
bt_pending() { [ -n "$bt_owner" ] && { bt_exists "$bt_owner" || bt_exists "$bt_owner.pending"; }; }

bt_test_clear() {
  # Проверяем здесь, даже если установленная ctl.sh ещё не знает о тестовом снимке.
  bt_test_dir=/tmp/pulsar-test
  [ -d "$bt_test_dir" ] && [ ! -L "$bt_test_dir" ] &&
    { bt_exists "$bt_test_dir/bt-reserved" || bt_exists "$bt_test_dir/bt-owner" ||
      bt_exists "$bt_test_dir/bt-owner.pending"; } &&
    bt_test_uid=$(id -u) &&
    [ "$(stat -c '%u:%a' "$bt_test_dir" 2>/dev/null)" = "$bt_test_uid:700" ] || return 0
  say 'Bluetooth тестового экземпляра не восстановлен: выполните test-stop'
  return 1
}

remember_bt_executable() {
  [ -n "$bt_executable" ] && [ -n "$bt_owner" ] || return 1
  bt_pinned_now && return 0
  # При замене бинаря не перепривязываем снимок к чужому inode.
  bt_protocol_inode= bt_protocol=
  bt_pending && return 1
  if [ -z "${PULSAR_BT_OWNER:-}" ]; then
    # Новый boot_id не должен применять флаги от предыдущей загрузки.
    for old in "$D"/.pulsar-bt-owner.*; do
      [ "$old" = "$bt_owner" ] || [ "$old" = "$bt_owner.pending" ] || rm -f "$old" || return 1
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

bt_protocol_supported() {
  probe_inode=$(stat -Lc '%d:%i' "$bt_executable") || return 2
  if [ "$probe_inode" != "$bt_protocol_inode" ]; then
    # Первый аргумент --version безопасен и для старого демона.
    probe_output=$(mktemp "$D/.bt-probe.XXXXXX") || return 2
    probe_result=2
    if "$bt_executable" --version --supervisor-protocol > "$probe_output" 2>/dev/null; then
      if printf 'pulsar-bt-managed/1\n' | cmp -s "$probe_output" -; then
        probe_result=0
      elif awk 'NR == 1 && /^pulsar [^[:space:]]+$/ { version = 1 }
        END { exit !(NR == 1 && version) }' "$probe_output"; then
        probe_result=1
      fi
    fi
    rm -f "$probe_output" || return 2
    [ "$probe_result" != 2 ] && [ "$probe_inode" = "$(stat -Lc '%d:%i' "$bt_executable")" ] || return 2
    bt_protocol_inode=$probe_inode bt_protocol=$probe_result
  fi
  return "$bt_protocol"
}

restore_bt() {
  restore_owner=${1:-$bt_owner}
  if [ -n "$restore_owner" ] && { bt_exists "$restore_owner" || bt_exists "$restore_owner.pending"; }; then
    if ! PULSAR_BT_OWNER=$restore_owner "$D/ctl.sh" bt-restore >> "$LOG" 2>&1; then
      say "Bluetooth restore failed; not starting another binary or the Yandex stack"
      return 1
    fi
    # Старый ctl мог вернуть успех, не зная о снимке или маркере передачи.
    for marker in "$restore_owner" "$restore_owner.pending"; do
      if bt_exists "$marker" &&
        [ "$(stat -c '%u' "$marker" 2>/dev/null)" = "$(id -u)" ]; then
        say "Bluetooth restore left an ownership marker in place"
        return 1
      fi
    done
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

# Запись находится в доверенном $D; starttime отличает ребёнка от повторно
# использованного PID. Ребёнок не проходит до exec без этой записи.
daemon_record=$D/.pulsar-daemon
launch_lock=$D/.pulsar-launch-lock

process_start() (
  case "$1" in ''|*[!0-9]*) return 1 ;; esac
  IFS= read -r ps_stat < "/proc/$1/stat" || return 1
  set -- ${ps_stat##*) }
  [ "$1" != Z ] && [ "$1" != X ] && [ -n "${20-}" ] && printf '%s\n' "${20}"
)

registered_daemon() {
  [ -f "$daemon_record" ] &&
    IFS=' ' read -r daemon_pid daemon_start daemon_extra < "$daemon_record" &&
    [ -z "$daemon_extra" ] && [ -n "$daemon_start" ] &&
    [ "$(process_start "$daemon_pid" 2>/dev/null)" = "$daemon_start" ]
}

launch_daemon() {
  # Сбросить унаследованную блокировку родителя и дождаться регистрации.
  exec 7>&-
  if [ "$have_flock" = 1 ]; then
    exec 7>>"$launch_lock" && flock -x 7 || exit 1
  fi
  IFS=' ' read -r child_pid _ < /proc/self/stat || exit 1
  while ! registered_daemon || [ "$daemon_pid" != "$child_pid" ]; do
    [ "$have_flock" = 0 ] &&
      [ "$(process_start "$$" 2>/dev/null)" = "$supervisor_start" ] || exit 1
    sleep 1
  done
  exec 7>&-
  PULSAR_BT_MANAGED=$bt_managed PULSAR_BT_OWNER=$bt_owner PULSAR_INSTALLED_BINARY=$installed_binary \
    exec "$launch" "$D/state.conf"
}

( while sleep 60; do rotate; done ) &
ROT=$!
# busybox ash does not run an EXIT trap on SIGTERM: clean up in the signal traps themselves.
trap 'kill $ROT 2>/dev/null; exit 143' TERM INT HUP
trap 'kill $ROT 2>/dev/null' EXIT

have_flock=0
if command -v flock >/dev/null 2>&1; then
  have_flock=1
else
  # Без flock ctl не может подтвердить BT-протокол. Регистрацию PID
  # всё равно надо дождаться: родитель может умереть до записи.
  supervisor_start=$(process_start "$$") || exit 1
fi

fails=0 swapped=0 backoff=$BACKOFF_MIN last_inode= bt_protocol_inode= bt_protocol=
bt_test_clear || exit 1
if [ -z "${PULSAR_BT_OWNER:-}" ] &&
  uid=$(id -u) &&
  { [ "$(stat -c '%u' "$bt_legacy" 2>/dev/null)" = "$uid" ] ||
    [ "$(stat -c '%u' "$bt_legacy.pending" 2>/dev/null)" = "$uid" ]; }; then
  # Старый снимок не связан с новым путём и не должен пережить новый захват.
  wait_restore_bt "$bt_legacy" || exit 0
fi
if [ -z "$bt_owner" ] && [ -n "$boot_id" ]; then
  # Если ctl заменили старой версией, снимок всё равно нельзя оставлять новому плееру.
  wait_restore_bt "$D/.pulsar-bt-owner.$boot_id" || exit 0
fi
while [ -f "$D/enabled" ]; do
  bt_test_clear || exit 1
  if [ -n "$bt_owner" ] && bt_exists "$bt_owner.pending"; then wait_restore_bt || exit 0; fi
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
    if [ "$have_flock" = 1 ] && [ "$bt_allowed" = 1 ] &&
      bt_lock_protocol=$("$D/ctl.sh" bt-restore --owner-lock 2>/dev/null) &&
      [ "$bt_lock_protocol" = pulsar-bt-owner-lock/2 ]; then
      # Шестибайтный снимок умеет возвращать ctl без блокировки.
      if [ "$(stat -c '%s' "$bt_owner" 2>/dev/null)" = 6 ]; then
        wait_restore_bt || exit 0
      fi
      if ! bt_pinned_now; then
        wait_restore_bt || exit 0
        remember_bt_executable || :
      fi
      protocol=1
      if bt_pinned_now; then bt_protocol_supported; protocol=$?; fi
      case "$protocol" in
        0)
          bt_managed=1
          # Рабочий путь могут заменить после последней сверки inode. Старый
          # демон, напротив, запускаем только по нему: его обновление читает /proc/self/exe.
          launch=$bt_executable installed_binary=$BIN
          ;;
        1) wait_restore_bt || exit 0 ;;
        2)
          # Сбой пробы не доказывает несовместимость: не возвращаем захваченный
          # player и не запускаем демон без BT поверх существующего снимка.
          if bt_pending; then sleep "$BACKOFF_MIN"; continue; fi
          ;;
      esac
    else
      wait_restore_bt || exit 0
    fi
    # Появился другой бинарь — он получает собственную попытку отката.
    if [ "$swapped" = 1 ]; then swapped=0 fails=0 backoff=$BACKOFF_MIN; fi
  fi
  if [ "$have_flock" = 1 ]; then
    (umask 077; : >> "$launch_lock") || exit 1
    exec 7>>"$launch_lock" || exit 1
    if ! flock -xn 7; then
      exec 7>&-
      sleep 1
      continue
    fi
  fi
  [ -f "$D/enabled" ] || exit 0
  # Другой ctl мог начать возврат после первой проверки, но до flock.
  if [ -n "$bt_owner" ] && bt_exists "$bt_owner.pending"; then
    exec 7>&-
    continue
  fi
  if [ "$inode" != "$(stat -Lc '%d:%i' "$BIN")" ] ||
    { [ "$bt_managed" = 1 ] && ! bt_pinned_now; }; then
    exec 7>&-
    continue
  fi
  bt_test_clear || exit 1
  if registered_daemon; then
    say "pulsar is still running (pid $daemon_pid); not starting another instance"
    exit 1
  fi
  last_inode=$inode
  start_at=$(date)
  t0=$(uptime_s)
  echo "=== start $start_at" >> "$LOG"
  launch_daemon >> "$LOG" 2>&1 &
  child=$!
  if ! child_start=$(process_start "$child" 2>/dev/null) ||
    ! (umask 077; printf '%s %s\n' "$child" "$child_start" > "$daemon_record.tmp" &&
      mv -f "$daemon_record.tmp" "$daemon_record"); then
    kill -KILL "$child" 2>/dev/null
    wait "$child"
    say 'could not register pulsar child'
    exit 1
  fi
  exec 7>&-
  wait "$child"
  rc=$?
  up=$(($(uptime_s) - t0))
  echo "=== exit $rc after ${up}s $(date)" >> "$LOG"
  if bt_pending && ! bt_exists "$bt_owner.pending"; then
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
