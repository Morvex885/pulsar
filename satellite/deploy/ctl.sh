#!/bin/sh
# pulsar control: enable | disable | restart | reload | status | log | api-key
#   enable  - stop the Yandex stack, run pulsar now and on every boot
#   disable - stop pulsar, start the Yandex stack again (also after reboot)
#   reload  - restart the supervisor and the daemon (after supervise.sh was replaced); never starts
#             the Yandex stack
#   api-key [base64|clear] - ESPHome Noise key ($D/api_key, base64 of 32 bytes, mode 0600): no
#             argument shows whether one is set; a value sets it, `clear` removes it; both restart.
#             recent Home Assistant (mid-2026+) provisions a key automatically over the API on first
#             connect; setting or clearing a different key by hand makes HA ask to re-authenticate.
#             A corrupt file makes the daemon fail closed, hence the validation below.
D=${PULSAR_DIR:-/data/pulsar}
Q=${PULSAR_Q:-/system/vendor/quasar/q}

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
  pidof supervise.sh >/dev/null 2>&1 && return 0
  (trap '' HUP; exec setsid "$D/supervise.sh" </dev/null >/dev/null 2>&1) &
  i=0
  while [ $i -lt 3 ] && ! pidof supervise.sh >/dev/null 2>&1; do
    sleep 1
    i=$((i + 1))
  done
}

# After stopping the daemon: the supervisor restarts it; start one if `enabled` but none runs.
restarted() {
  if [ -f "$D/enabled" ] && [ -x "$D/supervise.sh" ] && ! pidof supervise.sh >/dev/null 2>&1; then
    start_supervisor
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
  return 0
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
    if [ ! -x "$D/pulsar" ] || [ ! -x "$D/supervise.sh" ]; then
      echo "$D/pulsar or supervise.sh missing or not executable, nothing changed" >&2
      exit 1
    fi
    touch "$D/enabled"
    $Q stop >/dev/null 2>&1
    start_supervisor
    echo "enabled"
    ;;
  disable)
    rm -f "$D/enabled"
    stop_supervisors
    if ! stop_daemon; then
      echo "pulsar is still running, Yandex stack not started" >&2
      exit 1
    fi
    wait_supervisors_gone
    pidof quasar_launcher2 >/dev/null 2>&1 || $Q start >/dev/null 2>&1
    echo "disabled, Yandex stack started"
    ;;
  restart)
    if stop_daemon; then
      restarted
    else
      echo "pulsar did not exit" >&2
      exit 1
    fi
    ;;
  reload)
    stop_supervisors
    stop_daemon || { echo "pulsar did not exit" >&2; exit 1; }
    wait_supervisors_gone
    if [ -f "$D/enabled" ] && [ -x "$D/supervise.sh" ]; then
      start_supervisor
      echo "supervisor reloaded"
    else
      echo "supervisor stopped (not enabled)"
    fi
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
        rm -f "$D/api_key"
        echo "api key removed"
        stop_daemon || { echo "pulsar did not exit" >&2; exit 1; }
        restarted
        ;;
      *)
        valid_key "$2" || { echo "not a base64 encoding of 32 bytes (44 characters), nothing changed" >&2; exit 1; }
        (umask 077; printf '%s' "$2" > "$D/api_key.tmp") && chmod 600 "$D/api_key.tmp" && mv "$D/api_key.tmp" "$D/api_key" \
          || { rm -f "$D/api_key.tmp"; echo "cannot write $D/api_key" >&2; exit 1; }
        echo "api key set"
        stop_daemon || { echo "pulsar did not exit" >&2; exit 1; }
        restarted
        ;;
    esac
    ;;
  *)
    echo "usage: $0 enable|disable|restart|reload|status|log [lines]|api-key [base64|clear]"
    exit 1
    ;;
esac
