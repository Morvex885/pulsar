#!/bin/sh
# pulsar control: enable | disable | restart | reload | rollback | status | log | api-key | bt-restore
#   enable  - stop the Yandex stack, run pulsar now and on every boot
#   disable - stop pulsar, start the Yandex stack again (also after reboot)
#   reload  - restart the supervisor and the daemon (after supervise.sh was replaced); never starts
#             the Yandex stack
#   rollback - установить .prev после возврата штатного Bluetooth.
#   api-key [base64|clear] - ESPHome Noise key ($D/api_key, base64 of 32 bytes, mode 0600): no
#             argument shows whether one is set; a value sets it, `clear` removes it; both restart.
#             recent Home Assistant (mid-2026+) provisions a key automatically over the API on first
#             connect; setting or clearing a different key by hand makes HA ask to re-authenticate.
#             A corrupt file makes the daemon fail closed, hence the validation below.
#   bt-restore - вернуть флаги адаптера и штатный player после остановки pulsar.
D=${PULSAR_DIR:-/data/pulsar}
Q=${PULSAR_Q:-/system/vendor/quasar/q}
bt_legacy=/tmp/pulsar-bt-owner
bt_test_dir=/tmp/pulsar-test

bt_test_clear() {
  bt_test_owner=$bt_test_dir/bt-owner
  # Как в tools/station.sh: не доверяем снимку вне закрытого каталога нашего UID.
  if [ -d "$bt_test_dir" ] && [ ! -L "$bt_test_dir" ] &&
    bt_test_uid=$(id -u) && [ "$(stat -c '%u:%a' "$bt_test_dir" 2>/dev/null)" = "$bt_test_uid:700" ]; then
    if [ "${1-}" = --own ] && [ "${PULSAR_BT_OWNER:-}" = "$bt_test_owner" ]; then
      return 0
    fi
    [ -e "$bt_test_owner" ] || [ -L "$bt_test_owner" ] || return 0
    echo 'Bluetooth тестового экземпляра не восстановлен: выполните test-stop' >&2
    return 1
  fi
  if [ "${1-}" = --own ] && [ "${PULSAR_BT_OWNER:-}" = "$bt_test_owner" ]; then
    echo 'bt-restore: небезопасный каталог тестового снимка' >&2
    return 1
  fi
  return 0
}

bt_owner_path() {
  if [ -n "${PULSAR_BT_OWNER:-}" ]; then
    printf '%s\n' "$PULSAR_BT_OWNER"
    return
  fi
  IFS= read -r bt_boot < /proc/sys/kernel/random/boot_id || return 1
  case "$bt_boot" in ''|*[!0-9a-f-]*) return 1 ;; esac
  printf '%s/.pulsar-bt-owner.%s\n' "$D" "$bt_boot"
}

bt_for_owners() {
  bt_test_clear --own || return 1
  if [ -z "${PULSAR_BT_OWNER:-}" ]; then
    bt_owner=$bt_legacy
    "$@" || return 1
  fi
  bt_owner=$(bt_owner_path) || return 0
  "$@"
}

# SIGTERM, wait up to 10 s for the process to go, then SIGKILL. Returns 1 if it is still there.
stop_daemon() {
  pidof pulsar >/dev/null 2>&1 || return 0
  killall pulsar 2>/dev/null
  i=0
  while [ $i -lt 10 ] && pidof pulsar >/dev/null 2>&1; do sleep 1; i=$((i + 1)); done
  if pidof pulsar >/dev/null 2>&1; then
    echo "did not exit in 10 s, killing"
    killall -9 pulsar 2>/dev/null
    i=0
    while [ $i -lt 5 ] && pidof pulsar >/dev/null 2>&1; do sleep 1; i=$((i + 1)); done
  fi
  ! pidof pulsar >/dev/null 2>&1
}

# Starts supervise.sh unless one runs; waits until it does. Ignores SIGHUP: when called through
# `adb shell`, the session ends right after this script and would otherwise kill it before setsid().
start_supervisor() {
  bt_test_clear || return 1
  pidof supervise.sh >/dev/null 2>&1 && return 0
  (trap '' HUP; exec setsid "$D/supervise.sh" </dev/null >/dev/null 2>&1) &
  i=0
  while [ $i -lt 3 ] && ! pidof supervise.sh >/dev/null 2>&1; do
    sleep 1
    i=$((i + 1))
  done
  pidof supervise.sh >/dev/null 2>&1
}

# After stopping the daemon: the supervisor restarts it; start one if `enabled` but none runs.
restarted() {
  if [ -f "$D/enabled" ] && [ -x "$D/supervise.sh" ] && ! pidof supervise.sh >/dev/null 2>&1; then
    start_supervisor || { echo 'supervisor did not start' >&2; return 1; }
    echo "no supervisor was running, started one"
  else
    echo "supervisor restarts it in ~1 s"
  fi
}

# SIGTERM to every supervise.sh (incl. its log-rotation subshell); a supervisor only acts on it when
# its child exits, so call this before stop_daemon. SIGKILL after the wait if one lingers.
stop_supervisors() {
  pidof supervise.sh >/dev/null 2>&1 || return 0
  killall supervise.sh 2>/dev/null
}

wait_supervisors_gone() {
  i=0
  while [ $i -lt 5 ] && pidof supervise.sh >/dev/null 2>&1; do sleep 1; i=$((i + 1)); done
  pidof supervise.sh >/dev/null 2>&1 && { killall -9 supervise.sh 2>/dev/null; sleep 1; }
  ! pidof supervise.sh >/dev/null 2>&1
}

stop_service() {
  stop_supervisors
  stop_daemon || { echo "pulsar did not exit" >&2; return 1; }
  wait_supervisors_gone || { echo "supervisor is still running" >&2; return 1; }
}

bt_error() { echo "bt-restore: $* (снимок сохранён в $bt_owner)" >&2; return 1; }

bt_effective_uid() { awk '$1 == "Uid:" { print $3; exit }' "$1/status" 2>/dev/null; }

bt_player_running() {
  # Штатный init-скрипт сверяет и exe (-x), и имя процесса (-n), а не только pidfile.
  for bt_pid in "$bt_proc"/[0-9]*; do
    { IFS= read -r bt_comm < "$bt_pid/comm"; } 2>/dev/null || continue
    [ "$bt_comm" = bluealsa-aplay ] &&
      [ "$(readlink "$bt_pid/exe" 2>/dev/null)" = "$bt_player" ] &&
      [ "$(bt_effective_uid "$bt_pid")" = "$bt_uid" ] &&
      return 0
  done
  return 1
}

bt_bluez_owner() {
  bt_reply=$(dbus-send --system --print-reply --reply-timeout=2000 --dest=org.freedesktop.DBus \
    /org/freedesktop/DBus org.freedesktop.DBus.GetNameOwner string:org.bluez) || return 1
  printf '%s\n' "$bt_reply" | awk '
    $1 == "string" && NF == 2 && $2 ~ /^":[0-9]+(\.[0-9]+)+"$/ {
      owner = substr($2, 2, length($2) - 2); count++
    }
    END { if (count == 1) print owner; else exit 1 }
  '
}

bt_pin_bluez() {
  bt_bluez=$(bt_bluez_owner) || bt_error 'не удалось определить владельца org.bluez'
}

bt_same_bluez() {
  [ "$(bt_bluez_owner)" = "$bt_bluez" ] || bt_error 'владелец org.bluez сменился или недоступен'
}

bt_restore_flag() {
  bt_value=false
  [ "$2" = 0 ] || bt_value=true
  dbus-send --system --print-reply --reply-timeout=2000 "--dest=$bt_bluez" /org/bluez/hci0 \
    org.freedesktop.DBus.Properties.Set string:org.bluez.Adapter1 "string:$1" \
    "variant:boolean:$bt_value" >/dev/null || bt_error "не удалось вернуть $1"
}

bt_read_owner() {
  bt_flags=
  [ -L "$bt_owner" ] || [ -e "$bt_owner" ] || return 0
  bt_uid=$(id -u) || return 1
  if ! bt_meta=$(stat -c '%u:%a:%s' "$bt_owner" 2>/dev/null); then
    [ ! -L "$bt_owner" ] && [ ! -e "$bt_owner" ] && return 0
    bt_error 'не удалось проверить файл снимка'
    return 1
  fi
  # Чужой объект по старому пути в /tmp не мог появиться при нашем захвате.
  if [ "${bt_meta%%:*}" != "$bt_uid" ]; then
    [ "$bt_owner" != "$bt_test_owner" ] || { bt_error 'чужой файл тестового снимка'; return 1; }
    return 0
  fi
  [ ! -L "$bt_owner" ] && [ -f "$bt_owner" ] && [ "$bt_meta" = "$bt_uid:600:6" ] ||
    { bt_error 'некорректный файл снимка'; return 1; }
  # Точка сохраняет перевод строки при подстановке; читаем не более шести байт.
  bt_flags=$(head -c 6 "$bt_owner" && printf .) || return 1
  case "$bt_flags" in
    [01]' '[01]' '[01]'
.') bt_flags=${bt_flags%??} ;;
    *) bt_error 'некорректный формат снимка'; return 1 ;;
  esac
}

bt_stopped() {
  bt_proc=${PULSAR_BT_PROC:-/proc}
  bt_pids= bt_test_exe=
  if [ "${bt_owner##*/}" = bt-owner ]; then bt_test_exe=${bt_owner%/*}/pulsar; fi
  for bt_pid in $(pidof pulsar); do
    case "$bt_pid" in ''|*[!0-9]*) continue ;; esac
    [ "$(bt_effective_uid "$bt_proc/$bt_pid")" = "$bt_uid" ] || continue
    bt_exe=$(readlink "$bt_proc/$bt_pid/exe" 2>/dev/null) || continue
    case "${bt_exe%' (deleted)'}" in
      "$D/pulsar"|"$D"/.pulsar-bt-executable.*/pulsar|"$bt_test_exe") ;;
      *) continue ;;
    esac
    bt_pids=${bt_pids:+$bt_pids }$bt_pid
  done
  if [ "${1-}" = --self ]; then
    [ "$bt_pids" = "$PPID" ] || { bt_error 'запрос не от единственного процесса pulsar'; return 1; }
  elif [ -n "$bt_pids" ]; then
    bt_error 'pulsar ещё работает'
    return 1
  fi
}

bt_hide() {
  bt_read_owner || return 1
  [ -n "$bt_flags" ] || return 0
  bt_stopped || return 1
  # Обе записи должны попасть в один экземпляр BlueZ, даже если имя перехватили.
  bt_pin_bluez || return 1
  bt_ok=0
  bt_restore_flag Discoverable 0 || bt_ok=1
  bt_restore_flag Pairable 0 || bt_ok=1
  [ "$bt_ok" = 0 ] && bt_same_bluez
}

bt_restore() {
  bt_read_owner || return 1
  [ -n "$bt_flags" ] || return 0
  bt_stopped "${1-}" || return 1

  # Пространство пути подменяется только для изолированной проверки протокола.
  bt_init=${PULSAR_BT_INIT:-/etc/init.d/S53bluealsa-aplay}
  if [ "${bt_flags##* }" = 1 ]; then
    bt_player=$(readlink -f /usr/bin/bluealsa-aplay) &&
      [ -x "$bt_player" ] && [ -x "$bt_init" ] && [ -d "$bt_proc" ] ||
      { bt_error 'штатный bluealsa-aplay недоступен'; return 1; }
  fi
  bt_discover=${bt_flags#* }
  bt_pin_bluez &&
    bt_restore_flag Pairable "${bt_flags%% *}" &&
    bt_restore_flag Discoverable "${bt_discover%% *}" || return 1
  if [ "${bt_flags##* }" = 1 ] && ! bt_player_running; then
    bt_same_bluez || return 1
    "$bt_init" start || { bt_error 'init-скрипт не запустил bluealsa-aplay'; return 1; }
    bt_wait=0
    while ! bt_player_running; do
      [ "$bt_wait" -lt 3 ] || { bt_error 'процесс bluealsa-aplay не появился'; return 1; }
      sleep 1
      bt_wait=$((bt_wait + 1))
    done
  fi
  # В том числе если BlueZ сменился, пока поднимался штатный player.
  bt_same_bluez || return 1
  rm -f "$bt_owner" || bt_error 'не удалось удалить снимок'
}

rollback() {
  (
    cd "$D" || exit 1
    # Пока копия не готова, рабочую службу не останавливаем.
    trap 'rm -f pulsar.rb' EXIT
    rm -f pulsar.rb && cp -p pulsar.prev pulsar.rb && [ -x pulsar.rb ] || exit 1
    stop_service && bt_for_owners bt_restore || exit 1
    # Перед повторным использованием временного пути закрепляем действующий запрет захвата.
    if [ pulsar -ef pulsar.bt-blocked.next ]; then
      mv -f pulsar.bt-blocked.next pulsar.bt-blocked || { "$D/ctl.sh" reload; exit 1; }
    fi
    # Сохраняем текущий бинарь, не снимая его с рабочего пути до атомарной замены.
    if { [ pulsar -ef pulsar.bad ] || ln -f pulsar pulsar.bad; } &&
      rm -f pulsar.bt-blocked.next && ln pulsar.rb pulsar.bt-blocked.next &&
      [ pulsar.rb -ef pulsar.bt-blocked.next ] &&
      mv -f pulsar.rb pulsar; then
      # Пока запрет не перенесён на постоянный путь, supervisor учтёт временный.
      mv -f pulsar.bt-blocked.next pulsar.bt-blocked
      sync
      "$D/ctl.sh" reload
    else
      rm -f pulsar.bt-blocked.next
      echo 'rollback: бинарь не заменён; перезапускаем текущий' >&2
      "$D/ctl.sh" reload
      exit 1
    fi
  )
}

valid_key() { # 44 base64 chars (32 bytes + one '='), and 32 bytes when a decoder exists
  case "$1" in
    *[!A-Za-z0-9+/=]*) return 1 ;;
  esac
  [ ${#1} -eq 44 ] || return 1
  case "$1" in *=) ;; *) return 1 ;; esac
  if command -v base64 >/dev/null 2>&1; then
    [ "$(printf '%s' "$1" | base64 -d 2>/dev/null | wc -c)" -eq 32 ] || return 1
  fi
}

case "$1" in
  enable)
    bt_test_clear || exit 1
    if [ ! -x "$D/pulsar" ] || [ ! -x "$D/supervise.sh" ]; then
      echo "$D/pulsar or supervise.sh missing or not executable, nothing changed" >&2
      exit 1
    fi
    touch "$D/enabled"
    $Q stop >/dev/null 2>&1
    start_supervisor || { echo 'supervisor did not start' >&2; exit 1; }
    echo "enabled"
    ;;
  disable)
    bt_test_clear || exit 1
    rm -f "$D/enabled"
    stop_supervisors
    if ! stop_daemon; then
      echo "pulsar is still running, Yandex stack not started" >&2
      exit 1
    fi
    wait_supervisors_gone || { echo "supervisor is still running, Yandex stack not started" >&2; exit 1; }
    bt_for_owners bt_restore || exit 1
    bt_test_clear || exit 1
    pidof quasar_launcher2 >/dev/null 2>&1 || "$Q" start >/dev/null 2>&1 || exit 1
    echo "disabled, Yandex stack started"
    ;;
  restart)
    bt_test_clear || exit 1
    if stop_daemon; then
      restarted
    else
      echo "pulsar did not exit" >&2
      exit 1
    fi
    ;;
  reload)
    case "$#:${2-}" in 1:|2:--stop) ;; *) exit 2 ;; esac
    bt_test_clear || exit 1
    stop_service || exit 1
    if [ "${2-}" = --stop ]; then
      bt_for_owners bt_hide
      exit $?
    fi
    if [ -f "$D/enabled" ] && [ -x "$D/supervise.sh" ]; then
      # Новый supervisor дождётся возврата владения или перезапустит демон для
      # восстановления BlueZ. Ошибка закрытия окна не должна оставить сервис выключенным.
      bt_for_owners bt_hide || echo 'Bluetooth hide failed; restarting the supervisor to recover' >&2
      start_supervisor || { echo 'supervisor did not start' >&2; exit 1; }
      echo "supervisor reloaded"
    else
      bt_for_owners bt_hide || exit 1
      echo "supervisor stopped (not enabled)"
    fi
    ;;
  rollback)
    [ "$#" -eq 1 ] || exit 2
    bt_test_clear || exit 1
    rollback
    ;;
  status)
    [ -f "$D/enabled" ] && echo "mode: pulsar" || echo "mode: yandex"
    echo "pulsar pid: $(pidof pulsar)"
    echo "quasar pid: $(pidof quasar_launcher2 maind)"
    ;;
  log)
    tail -n "${2:-50}" /tmp/pulsar.log
    ;;
  api-key)
    case "${2-}" in
      "")
        [ -s "$D/api_key" ] && echo "api key: set" || echo "api key: not set (plaintext API)"
        ;;
      clear)
        bt_test_clear || exit 1
        rm -f "$D/api_key"
        echo "api key removed"
        stop_daemon || { echo "pulsar did not exit" >&2; exit 1; }
        restarted
        ;;
      *)
        valid_key "$2" || { echo "not a base64 encoding of 32 bytes (44 characters), nothing changed" >&2; exit 1; }
        bt_test_clear || exit 1
        (umask 077; printf '%s' "$2" > "$D/api_key.tmp") && chmod 600 "$D/api_key.tmp" && mv "$D/api_key.tmp" "$D/api_key" \
          || { rm -f "$D/api_key.tmp"; echo "cannot write $D/api_key" >&2; exit 1; }
        echo "api key set"
        stop_daemon || { echo "pulsar did not exit" >&2; exit 1; }
        restarted
        ;;
    esac
    ;;
  bt-restore)
    case "$#:${2-}" in
      1:) bt_for_owners bt_restore ;;
      2:--self) bt_for_owners bt_restore --self ;;
      2:--hide) bt_for_owners bt_hide ;;
      2:--owner-path) bt_owner_path ;;
      2:--supported) : ;;
      2:--check) command -v dbus-send >/dev/null 2>&1 ;;
      *) exit 2 ;;
    esac
    ;;
  *)
    echo "usage: $0 enable|disable|restart|reload [--stop]|rollback|status|log [lines]|api-key [base64|clear]|bt-restore"
    exit 1
    ;;
esac
