#!/bin/sh
# Запуск из любого каталога: sh satellite/deploy/test-bt-ownership.sh
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
station=$here/../../tools/station.sh
BT_FIXTURE=$(mktemp -d)
cleanup() {
  p=${test_pid_for_cleanup:-}
  if [ -s "$BT_FIXTURE/test/pid" ]; then read -r p < "$BT_FIXTURE/test/pid"; fi
  if [ -n "$p" ] && [ "$("$REAL_READLINK" "/proc/$p/exe" 2>/dev/null)" = "$BT_FIXTURE/test/pulsar" ]; then
    kill -KILL "$p"
  fi
  rm -rf "$BT_FIXTURE"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
REAL_READLINK=$(command -v readlink)
REAL_CP=$(command -v cp)
REAL_SLEEP=$(command -v sleep)
REAL_MV=$(command -v mv)
REAL_STAT=$(command -v stat)
REAL_DATE=$(command -v date)
REAL_PIDOF=$(command -v pidof)
export BT_FIXTURE REAL_READLINK REAL_SLEEP REAL_MV REAL_STAT REAL_CP
export REAL_DATE REAL_PIDOF
export PULSAR_DIR=$BT_FIXTURE/data PULSAR_LOG=$BT_FIXTURE/supervise.log PULSAR_Q=$BT_FIXTURE/bin/quasar
export PULSAR_BT_OWNER=$BT_FIXTURE/owner PULSAR_BT_PROC=$BT_FIXTURE/proc PULSAR_BT_INIT=$BT_FIXTURE/bin/init
export PULSAR_TAG=isolated PULSAR_LITE2=station.invalid
export XDG_CONFIG_HOME=$BT_FIXTURE/config
unset PULSAR_BT_MANAGED PULSAR_LONG
mkdir -p "$BT_FIXTURE/bin" "$BT_FIXTURE/test" "$BT_FIXTURE/lock" "$PULSAR_BT_PROC" "$PULSAR_DIR"
chmod 700 "$BT_FIXTURE/test"
printf '%s\n' isolated > "$BT_FIXTURE/lock/owner"
events=$BT_FIXTURE/events
: > "$events"
: > "$BT_FIXTURE/bluealsa-aplay"
chmod +x "$BT_FIXTURE/bluealsa-aplay"

cat > "$BT_FIXTURE/bin/readlink" <<'EOF'
#!/bin/sh
if [ "$*" = '-f /usr/bin/bluealsa-aplay' ]; then
  echo "$BT_FIXTURE/bluealsa-aplay"
else
  exec "$REAL_READLINK" "$@"
fi
EOF
cat > "$BT_FIXTURE/bin/stat" <<'EOF'
#!/bin/sh
# Simulate a different UID without depending on root/chown on the test host.
if [ "$1" = -c ] &&
   { { [ "${FOREIGN_OWNER:-0}" = 1 ] && [ "$3" = "${FOREIGN_MARKER:-$PULSAR_BT_OWNER}" ]; } ||
     { [ "${FOREIGN_TEST_DIR:-0}" = 1 ] && [ "$3" = "$BT_FIXTURE/test" ]; }; }; then
  meta=$("$REAL_STAT" "$@") || exit 1
  case "$meta" in *:*) echo "-1:${meta#*:}" ;; *) echo -1 ;; esac
else
  exec "$REAL_STAT" "$@"
fi
EOF
cat > "$BT_FIXTURE/bin/dbus-send" <<'EOF'
#!/bin/sh
set -eu
if [ "$*" = '--system --print-reply --reply-timeout=2000 --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.GetNameOwner string:org.bluez' ]; then
  owner=:1.41
  if [ -f "$BT_FIXTURE/bluez-owner" ]; then read -r owner < "$BT_FIXTURE/bluez-owner"; fi
  printf 'method return time=0 sender=org.freedesktop.DBus -> destination=:1.99 serial=3 reply_serial=2\n   string "%s"\n' "$owner"
  exit 0
fi
case "$4" in --dest=:1.41|--dest=:1.42) ;; *) exit 2 ;; esac
[ "$*" = "--system --print-reply --reply-timeout=2000 $4 /org/bluez/hci0 org.freedesktop.DBus.Properties.Set string:org.bluez.Adapter1 $8 $9" ]
prop=${8#string:}
value=${9#variant:boolean:}
printf '%s:%s\n' "$prop" "$value" >> "$BT_FIXTURE/events"
if [ "${TRACE_DBUS_ROUTES:-0}" = 1 ]; then printf '%s:%s\n' "${4#--dest=}" "$prop" >> "$BT_FIXTURE/dbus-routes"; fi
if [ "$prop" = "${SHIFT_BLUEZ_AT-}" ]; then echo :1.42 > "$BT_FIXTURE/bluez-owner"; fi
if [ "$prop" = "${FAIL_FLAG-}" ] && { [ -z "${FAIL_VALUE-}" ] || [ "$value" = "$FAIL_VALUE" ]; }; then
  if [ "${FAIL_FLAG_ONCE:-0}" = 1 ]; then
    [ ! -e "$BT_FIXTURE/flag-failed" ] || exit 0
    touch "$BT_FIXTURE/flag-failed"
  fi
  exit 1
fi
EOF
cat > "$PULSAR_BT_INIT" <<'EOF'
#!/bin/sh
set -eu
printf 'init:%s\n' "$1" >> "$BT_FIXTURE/events"
p=$PULSAR_BT_PROC/101
case "$1" in
  stop) [ "${FAIL_STOP:-0}" = 0 ] && rm -rf "$p" ;;
  start)
    if [ "${FAIL_START_ONCE:-0}" = 1 ] && [ ! -e "$BT_FIXTURE/start-failed" ]; then
      touch "$BT_FIXTURE/start-failed"
      exit 1
    fi
    [ "${FAIL_START:-0}" = 0 ] || exit 1
    [ "${NO_SPAWN:-0}" = 0 ] || exit 0
    mkdir -p "$p"
    uid=$(id -u)
    printf 'Uid:\t%s\t%s\t%s\t%s\n' "$uid" "$uid" "$uid" "$uid" > "$p/status"
    printf '%s\n' bluealsa-aplay > "$p/comm"
    ln -s "$BT_FIXTURE/bluealsa-aplay" "$p/exe"
    ;;
  *) exit 2 ;;
esac
EOF
cat > "$BT_FIXTURE/bin/pidof" <<'EOF'
#!/bin/sh
case "$*" in
  pulsar)
    if [ -n "${SELF_PID-}" ]; then echo "$SELF_PID ${EXTRA_PIDS-}"; exit 0; fi
    if [ -f "$BT_FIXTURE/pulsar-running" ]; then echo "501 ${EXTRA_PIDS-}"; exit 0; fi
    if [ -n "${EXTRA_PIDS-}" ]; then echo "$EXTRA_PIDS"; exit 0; fi
    if [ -s "$BT_FIXTURE/test/pid" ]; then
      read -r p < "$BT_FIXTURE/test/pid"
      if [ "$(readlink "/proc/$p/exe" 2>/dev/null)" = "$BT_FIXTURE/test/pulsar" ]; then echo "$p"; exit 0; fi
    fi ;;
  supervise.sh) if [ -f "$BT_FIXTURE/supervisor-running" ]; then echo 502; exit 0; fi ;;
  'quasar_launcher2 maind') [ ! -f "$BT_FIXTURE/quasar-running" ] || { echo 503; exit 0; } ;;
esac
exit 1
EOF
cat > "$BT_FIXTURE/bin/killall" <<'EOF'
#!/bin/sh
case "$*" in
  pulsar|'-9 pulsar')
    if [ -n "${LATE_ACQUIRE-}" ] && [ -f "$BT_FIXTURE/pulsar-running" ]; then
      (umask 077; printf '%s\n' "$LATE_ACQUIRE" > "$PULSAR_BT_OWNER") || exit 1
      "$PULSAR_BT_INIT" stop || exit 1
    fi
    rm -f "$BT_FIXTURE/pulsar-running" ;;
  supervise.sh|'-9 supervise.sh') rm -f "$BT_FIXTURE/supervisor-running" ;;
  *) exit 2 ;;
esac
EOF
cat > "$PULSAR_Q" <<'EOF'
#!/bin/sh
printf 'quasar:%s\n' "$1" >> "$BT_FIXTURE/events"
EOF
cat > "$BT_FIXTURE/bin/sleep" <<'EOF'
#!/bin/sh
# A real handshake below, not this delay, proves the test process reached its ready state.
case "$1" in 60|2) exec "$REAL_SLEEP" 0.15 ;; 1|3|6|12|24|30) exec "$REAL_SLEEP" 0.01 ;; esac
EOF
printf '#!/bin/sh\nexit 1\n' > "$BT_FIXTURE/bin/ipcs"
cat > "$BT_FIXTURE/bin/date" <<'EOF'
#!/bin/sh
if [ "${SHADOW_AT_START:-0}" = 1 ] && [ -e "$BT_FIXTURE/late-replacement" ]; then
  "$REAL_MV" "$BT_FIXTURE/late-replacement" "$PULSAR_DIR/pulsar" || exit 1
  echo 'install:renamed' >> "$BT_FIXTURE/events"
fi
exec "$REAL_DATE" "$@"
EOF
cat > "$BT_FIXTURE/bin/setsid" <<'EOF'
#!/bin/sh
[ "$*" = "$PULSAR_DIR/supervise.sh" ] || exit 2
echo 'supervisor:start' >> "$BT_FIXTURE/events"
touch "$BT_FIXTURE/supervisor-running"
EOF
cat > "$BT_FIXTURE/bin/mv" <<'EOF'
#!/bin/sh
if [ "${FAIL_REPLACE:-0}" = 1 ] && [ "$*" = '-f pulsar.rb pulsar' ]; then exit 1; fi
if [ "${FAIL_STATION_REPLACE:-0}" = 1 ]; then
  case "$*" in '-f pulsar.new pulsar'|'-f .station-replace.'*'/pulsar pulsar') exit 1 ;; esac
fi
exec "$REAL_MV" "$@"
EOF
cat > "$BT_FIXTURE/bin/cp" <<'EOF'
#!/bin/sh
if [ "${FAIL_STATION_COPY:-0}" = 1 ]; then
  case "$*" in '-p pulsar.orig .station-replace.'*) exit 1 ;; esac
fi
exec "$REAL_CP" "$@"
EOF
cat > "$BT_FIXTURE/bin/df" <<'EOF'
#!/bin/sh
printf '%s\n' 'Filesystem 1024-blocks Used Available Capacity Mounted on' 'fixture 100000 0 100000 0% /data'
EOF
cat > "$BT_FIXTURE/bin/ssh" <<'EOF'
#!/bin/sh
set -eu
while [ "$1" = -o ]; do shift 2; done
[ "$1" = station.invalid ]
shift
if [ "$1" = sh ]; then
  for arg do
    shift
    case "$arg" in /tmp/pulsar-test) arg=$BT_FIXTURE/test ;; /data/pulsar) arg=$PULSAR_DIR ;; esac
    set -- "$@" "$arg"
  done
  exec "$@"
fi
cmd=$(printf '%s\n' "$1" | sed "s|/tmp/pulsar-agent.lock|$BT_FIXTURE/lock|g; s|/tmp/pulsar-test|$BT_FIXTURE/test|g; s|/data/pulsar|$PULSAR_DIR|g; s|/tmp/pulsar-bt-owner|$PULSAR_BT_OWNER|g")
exec sh -c "$cmd"
EOF
chmod +x "$BT_FIXTURE"/bin/*
export PATH=$BT_FIXTURE/bin:$PATH
for script in ctl.sh supervise.sh; do
  sed "s|/tmp/pulsar-test|$BT_FIXTURE/test|g" "$here/$script" > "$PULSAR_DIR/$script"
done
chmod +x "$PULSAR_DIR/ctl.sh" "$PULSAR_DIR/supervise.sh"
ctl() { "$PULSAR_DIR/ctl.sh" "$@"; }
supervise() { sh "$PULSAR_DIR/supervise.sh" "$@"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
reject() { if "$@"; then fail "expected failure: $*"; fi; }
expect() { [ "$(cat "$events")" = "$1" ] || fail "expected [$1]; got [$(cat "$events")]"; }
marker() { (umask 077; printf '%s\n' "$1" > "$PULSAR_BT_OWNER"); }
trace() { : > "$events"; }
await_test_ready() {
  i=0
  while [ ! -f "$BT_FIXTURE/test-ready" ] && [ "$i" -lt 100 ]; do
    "$REAL_SLEEP" 0.05
    i=$((i + 1))
  done
  [ -f "$BT_FIXTURE/test-ready" ] || fail 'test daemon never became ready'
  rm "$BT_FIXTURE/test-ready"
}
install_fixture() {
  cp "$1" "$PULSAR_DIR/pulsar.new"
  mv -f "$PULSAR_DIR/pulsar.new" "$PULSAR_DIR/pulsar"
}

fake_pulsar() {
  p=$PULSAR_BT_PROC/$1
  mkdir -p "$p"
  printf 'Uid:\t%s\t%s\t%s\t%s\n' "$2" "$2" "$2" "$2" > "$p/status"
  ln -s "$3" "$p/exe"
}

# SSH-адрес из окружения имеет приоритет; без него читается только одна строка файла.
mkdir -p "$XDG_CONFIG_HOME/pulsar/stations"
station_file=$XDG_CONFIG_HOME/pulsar/stations/lite2
printf '%s\n' 'not-the-env-host.invalid' > "$station_file"
sh "$station" lite2 status >/dev/null
printf '%s\n' station.invalid > "$station_file"
PULSAR_LITE2= sh "$station" lite2 status >/dev/null
printf '%s\n' station.invalid extra > "$station_file"
invalid=$(PULSAR_LITE2= sh "$station" lite2 status 2>&1) && fail 'accepted a multi-line destination'
case "$invalid" in *'ожидается одна строка с SSH-назначением'*) ;; *) fail "$invalid" ;; esac

# Пробник и восстановление без снимка ничего не мутируют.
ctl bt-restore --supported
ctl bt-restore --check
ctl bt-restore
expect ''
marker '1 0 1'
chmod 644 "$PULSAR_BT_OWNER"
reject ctl bt-restore
reject ctl disable
expect ''
chmod 600 "$PULSAR_BT_OWNER"
printf '1 0 1\nextra\n' > "$PULSAR_BT_OWNER"
reject ctl bt-restore
expect ''
[ -f "$PULSAR_BT_OWNER" ]
printf '1 0 10' > "$PULSAR_BT_OWNER"
reject ctl bt-restore
expect ''
marker '0 1 0'
fake_pulsar 501 "$(id -u)" "$PULSAR_DIR/pulsar (deleted)"
fake_pulsar 504 "$(($(id -u) + 1))" "$PULSAR_DIR/pulsar"
fake_pulsar 505 "$(id -u)" "$BT_FIXTURE/unrelated/pulsar"
touch "$BT_FIXTURE/pulsar-running"
reject ctl bt-restore
reject ctl bt-restore --self
rm "$BT_FIXTURE/pulsar-running"
expect ''
EXTRA_PIDS='504 505' sh -c '
  p=$PULSAR_BT_PROC/$$
  mkdir "$p" && cp "/proc/$$/status" "$p/status" &&
    ln -s "$PULSAR_DIR/pulsar" "$p/exe" || exit 1
  SELF_PID=$$ "$1" bt-restore --self
  rc=$?
  rm -rf "$p"
  exit "$rc"
' fixture "$PULSAR_DIR/ctl.sh"
expect 'Pairable:false
Discoverable:true'
trace
marker '0 1 0'
EXTRA_PIDS='504 505' ctl bt-restore
expect 'Pairable:false
Discoverable:true'
[ ! -e "$PULSAR_BT_OWNER" ] || fail 'unrelated processes blocked stock restoration'
trace

# Если имя BlueZ перешло другому процессу, обе записи адресованы прежнему
# владельцу, а снимок остаётся до повторной попытки.
marker '1 0 1'
reject env TRACE_DBUS_ROUTES=1 SHIFT_BLUEZ_AT=Pairable "$PULSAR_DIR/ctl.sh" bt-restore
expect 'Pairable:true
Discoverable:false'
[ "$(cat "$BT_FIXTURE/dbus-routes")" = ':1.41:Pairable
:1.41:Discoverable' ] && [ -f "$PULSAR_BT_OWNER" ] ||
  fail 'restore mixed BlueZ owners or consumed the snapshot after an owner change'
trace
: > "$BT_FIXTURE/dbus-routes"
TRACE_DBUS_ROUTES=1 ctl bt-restore
expect 'Pairable:true
Discoverable:false
init:start'
[ "$(cat "$BT_FIXTURE/dbus-routes")" = ':1.42:Pairable
:1.42:Discoverable' ] && [ ! -e "$PULSAR_BT_OWNER" ] ||
  fail 'restore did not retry against the new BlueZ owner'
"$PULSAR_BT_INIT" stop
rm "$BT_FIXTURE/bluez-owner"
trace
: > "$BT_FIXTURE/dbus-routes"
marker '1 0 0'
reject env TRACE_DBUS_ROUTES=1 SHIFT_BLUEZ_AT=Discoverable "$PULSAR_DIR/ctl.sh" bt-restore --hide
expect 'Discoverable:false
Pairable:false'
[ "$(cat "$BT_FIXTURE/dbus-routes")" = ':1.41:Discoverable
:1.41:Pairable' ] && [ -f "$PULSAR_BT_OWNER" ] ||
  fail 'hide mixed BlueZ owners or consumed the snapshot'
rm "$BT_FIXTURE/bluez-owner" "$PULSAR_BT_OWNER"
trace

# Обычный рестарт не применяет снимок. Disable возвращает только исходные флаги/процесс.
marker '0 1 0'
touch "$PULSAR_DIR/enabled" "$BT_FIXTURE/pulsar-running" "$BT_FIXTURE/supervisor-running"
ctl restart
expect ''
[ -f "$PULSAR_BT_OWNER" ]
touch "$BT_FIXTURE/pulsar-running"
reject env FAIL_FLAG=Discoverable "$PULSAR_DIR/ctl.sh" reload --stop
expect 'Discoverable:false
Pairable:false'
[ ! -e "$BT_FIXTURE/supervisor-running" ] || fail 'failed stop restarted production'
trace
FAIL_FLAG=Discoverable ctl reload
expect 'Discoverable:false
Pairable:false
supervisor:start'
[ -f "$BT_FIXTURE/supervisor-running" ] || fail 'ordinary reload stranded production'
trace
ctl reload --stop
expect 'Discoverable:false
Pairable:false'
trace
[ -f "$PULSAR_BT_OWNER" ]
[ -f "$PULSAR_DIR/enabled" ]
touch "$BT_FIXTURE/pulsar-running"
ctl disable
expect 'Pairable:false
Discoverable:true
quasar:start'
[ ! -e "$PULSAR_BT_OWNER" ] && [ ! -e "$PULSAR_DIR/enabled" ] || fail 'disable left ownership pending'

# A valid-looking marker from a different UID must not block return to stock or be removed.
marker '1 0 1'
trace
touch "$PULSAR_DIR/enabled" "$BT_FIXTURE/pulsar-running" "$BT_FIXTURE/supervisor-running"
FOREIGN_OWNER=1 ctl disable
expect 'quasar:start'
[ "$(cat "$PULSAR_BT_OWNER")" = '1 0 1' ] && [ ! -e "$PULSAR_DIR/enabled" ] ||
  fail 'disable removed or honored a foreign marker'
trace
touch "$PULSAR_DIR/enabled"
reject env FOREIGN_OWNER=1 "$PULSAR_DIR/supervise.sh"
expect 'quasar:start'
[ "$(cat "$PULSAR_BT_OWNER")" = '1 0 1' ] && [ ! -e "$PULSAR_DIR/enabled" ] ||
  fail 'automatic fallback removed or honored a foreign marker'
rm "$PULSAR_BT_OWNER"
ln -s "$BT_FIXTURE/nonexistent" "$PULSAR_BT_OWNER"
trace
FOREIGN_OWNER=1 ctl disable
expect 'quasar:start'
[ -L "$PULSAR_BT_OWNER" ] || fail 'disable removed a foreign symlink'
rm "$PULSAR_BT_OWNER"

# После неудачного stop запущенный исходный экземпляр не дублируется при возврате.
"$PULSAR_BT_INIT" start
marker '1 0 1'
reject env FAIL_STOP=1 "$PULSAR_BT_INIT" stop
trace
ctl bt-restore
expect 'Pairable:true
Discoverable:false'
[ -d "$PULSAR_BT_PROC/101" ]
"$PULSAR_BT_INIT" stop
trace

# Ни чужой exe, ни тот же exe под чужим UID не являются штатным player.
mkdir -p "$PULSAR_BT_PROC/102"
printf '%s\n' bluealsa-aplay > "$PULSAR_BT_PROC/102/comm"
ln -s /elsewhere/bluealsa-aplay "$PULSAR_BT_PROC/102/exe"
mkdir -p "$PULSAR_BT_PROC/103"
printf '%s\n' bluealsa-aplay > "$PULSAR_BT_PROC/103/comm"
ln -s "$BT_FIXTURE/bluealsa-aplay" "$PULSAR_BT_PROC/103/exe"
other_uid=$(($(id -u) + 1))
printf 'Uid:\t%s\t%s\t%s\t%s\n' "$other_uid" "$other_uid" "$other_uid" "$other_uid" > "$PULSAR_BT_PROC/103/status"
marker '1 0 1'
reject env FAIL_FLAG=Pairable "$PULSAR_DIR/ctl.sh" disable
expect 'Pairable:true'
[ -f "$PULSAR_BT_OWNER" ]
trace
reject env NO_SPAWN=1 "$PULSAR_DIR/ctl.sh" disable
expect 'Pairable:true
Discoverable:false
init:start'
[ -f "$PULSAR_BT_OWNER" ]
trace
ctl disable
expect 'Pairable:true
Discoverable:false
init:start
quasar:start'
[ -d "$PULSAR_BT_PROC/101" ] && [ -d "$PULSAR_BT_PROC/102" ] && [ -d "$PULSAR_BT_PROC/103" ] &&
  [ ! -e "$PULSAR_BT_OWNER" ] || fail 'stock process not restored or unrelated process affected'

# Краш-цикл меняет бинарь только после восстановления, не возвращая player между попытками.
cat > "$PULSAR_DIR/pulsar" <<'EOF'
#!/bin/sh
if [ ! -e "$PULSAR_BT_OWNER" ]; then
  (umask 077; printf '1 1 1\n' > "$PULSAR_BT_OWNER") || exit 2
  "$PULSAR_BT_INIT" stop || exit 2
fi
printf 'new:%s\n' "$PULSAR_BT_MANAGED" >> "$BT_FIXTURE/events"
exit 1
EOF
cp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/new"
cat > "$PULSAR_DIR/pulsar.prev" <<'EOF'
#!/bin/sh
printf 'prev:%s\n' "$PULSAR_BT_MANAGED" >> "$BT_FIXTURE/events"
if [ -f "$BT_FIXTURE/prev-restarted" ]; then
  mv "$BT_FIXTURE/corrected" "$PULSAR_DIR/pulsar"
else
  touch "$BT_FIXTURE/prev-restarted"
fi
touch "$BT_FIXTURE/pulsar-running" "$BT_FIXTURE/supervisor-running"
"$PULSAR_DIR/ctl.sh" restart
EOF
cat > "$BT_FIXTURE/corrected" <<'EOF'
#!/bin/sh
printf 'corrected:%s\n' "$PULSAR_BT_MANAGED" >> "$BT_FIXTURE/events"
rm -f "$PULSAR_DIR/enabled" "$BT_FIXTURE/supervisor-running"
EOF
chmod +x "$PULSAR_DIR/pulsar" "$PULSAR_DIR/pulsar.prev" "$BT_FIXTURE/corrected"
loops=$(for i in 1 2 3 4 5; do printf 'new:1\nDiscoverable:false\nPairable:false\n'; done)
trace
touch "$PULSAR_DIR/enabled"
FAIL_START_ONCE=1 supervise
expect "init:stop
$loops
Pairable:true
Discoverable:true
init:start
Pairable:true
Discoverable:true
init:start
prev:0
prev:0
corrected:1"
[ ! -e "$PULSAR_BT_OWNER" ]

# Ручной откат (в т.ч. из установщика): отказ копии не гасит текущий сервис,
# отказ замены после возврата Bluetooth запускает незаменённый бинарь.
cat > "$PULSAR_DIR/pulsar.prev" <<'EOF'
#!/bin/sh
printf 'manual-prev:%s\n' "$PULSAR_BT_MANAGED" >> "$BT_FIXTURE/events"
rm -f "$PULSAR_DIR/enabled"
EOF
chmod +x "$PULSAR_DIR/pulsar.prev"
cp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/before-manual"
touch "$PULSAR_DIR/enabled" "$BT_FIXTURE/supervisor-running" "$BT_FIXTURE/pulsar-running"
marker '1 0 0'
mv "$PULSAR_DIR/pulsar.prev" "$BT_FIXTURE/manual-prev"
trace
reject ctl rollback
expect ''
[ -f "$BT_FIXTURE/pulsar-running" ] && [ -f "$BT_FIXTURE/supervisor-running" ] ||
  fail 'rollback stopped the service before its replacement was ready'
mv "$BT_FIXTURE/manual-prev" "$PULSAR_DIR/pulsar.prev"
reject env FAIL_FLAG=Discoverable "$PULSAR_DIR/ctl.sh" rollback
expect 'Pairable:true
Discoverable:false'
[ ! -e "$BT_FIXTURE/supervisor-running" ] && [ -f "$PULSAR_BT_OWNER" ] &&
  cmp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/before-manual" || fail 'manual rollback ran after failed restore'
trace
reject env FAIL_REPLACE=1 "$PULSAR_DIR/ctl.sh" rollback
expect 'Pairable:true
Discoverable:false
supervisor:start'
cmp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/before-manual"
[ ! -e "$PULSAR_BT_OWNER" ]
trace
ctl rollback
expect 'supervisor:start'
[ "$PULSAR_DIR/pulsar" -ef "$PULSAR_DIR/pulsar.bt-blocked" ]
trace
reject env FAIL_REPLACE=1 "$PULSAR_DIR/ctl.sh" rollback
expect 'supervisor:start'
[ "$PULSAR_DIR/pulsar" -ef "$PULSAR_DIR/pulsar.bt-blocked" ] ||
  fail 'failed second rollback lost the current Bluetooth guard'
rm "$BT_FIXTURE/supervisor-running"
trace
supervise
expect 'manual-prev:0'
install_fixture "$BT_FIXTURE/before-manual"
trace
touch "$PULSAR_DIR/enabled"
PULSAR_BT_MANAGED=0 supervise
expect 'corrected:0'

# Ветка give_back без пригодной .prev использует тот же протокол.
rm -f "$PULSAR_DIR/pulsar" "$PULSAR_DIR/pulsar.prev"
marker '0 0 0'
trace
touch "$PULSAR_DIR/enabled"
reject supervise
expect 'Pairable:false
Discoverable:false
quasar:start'
[ ! -e "$PULSAR_BT_OWNER" ] && [ ! -e "$PULSAR_DIR/enabled" ] || fail 'give_back left ownership pending'

# A normal HA-style rename may install a pre-Bluetooth release with no private CLI flags.
cat > "$BT_FIXTURE/replacing" <<'EOF'
#!/bin/sh
[ "$PULSAR_BT_MANAGED" = 1 ] || exit 2
(umask 077; printf '1 0 1\n' > "$PULSAR_BT_OWNER") || exit 2
"$PULSAR_BT_INIT" stop || exit 2
mv "$BT_FIXTURE/stable.new" "$PULSAR_DIR/pulsar" || exit 2
printf 'updated\n' >> "$BT_FIXTURE/events"
EOF
cat > "$BT_FIXTURE/stable" <<'EOF'
#!/bin/sh
[ "$#" = 1 ] && [ "$1" = "$PULSAR_DIR/state.conf" ] &&
  [ ! -e "$PULSAR_BT_OWNER" ] && [ -d "$PULSAR_BT_PROC/101" ] || exit 2
printf 'stable\n' >> "$BT_FIXTURE/events"
rm -f "$PULSAR_DIR/enabled"
EOF
chmod +x "$BT_FIXTURE/replacing" "$BT_FIXTURE/stable"
for fail_flag in '' Pairable; do
  install_fixture "$BT_FIXTURE/replacing"
  cp "$BT_FIXTURE/stable" "$BT_FIXTURE/stable.new"
  trace
  touch "$PULSAR_DIR/enabled"
  if [ -n "$fail_flag" ]; then
    FAIL_FLAG="$fail_flag" FAIL_VALUE=true FAIL_FLAG_ONCE=1 supervise
    expect 'init:stop
updated
Discoverable:false
Pairable:false
Pairable:true
Pairable:true
Discoverable:false
init:start
stable'
  else
    supervise
    expect 'init:stop
updated
Discoverable:false
Pairable:false
Pairable:true
Discoverable:false
init:start
stable'
  fi
done

# A fresh supervisor restores unknown lineage, retries transient failures and
# waits for the stock player before starting the replacement.
install_fixture "$BT_FIXTURE/stable"
"$PULSAR_BT_INIT" stop
marker '1 0 1'
rm "$BT_FIXTURE/flag-failed"
trace
touch "$PULSAR_DIR/enabled"
FAIL_FLAG=Pairable FAIL_FLAG_ONCE=1 supervise
expect 'Pairable:true
Pairable:true
Discoverable:false
init:start
stable'

# A fresh supervisor around the same executable must not reopen the stock path.
cat > "$BT_FIXTURE/same" <<'EOF'
#!/bin/sh
set -eu
if [ ! -e "$PULSAR_BT_OWNER" ]; then
  (umask 077; printf '1 0 1\n' > "$PULSAR_BT_OWNER")
  "$PULSAR_BT_INIT" stop
fi
[ ! -e "$PULSAR_BT_PROC/101" ]
printf 'same:%s\n' "$PULSAR_BT_MANAGED" >> "$BT_FIXTURE/events"
rm "$PULSAR_DIR/enabled"
EOF
chmod +x "$BT_FIXTURE/same"
install_fixture "$BT_FIXTURE/same"
trace
touch "$PULSAR_DIR/enabled"
supervise
expect 'init:stop
same:1
Discoverable:false
Pairable:false'
trace
touch "$PULSAR_DIR/enabled"
supervise
expect 'same:1
Discoverable:false
Pairable:false'
ctl bt-restore

# Новый supervisor со старым ctl не даёт демону захватывать проигрыватель.
mv "$PULSAR_DIR/ctl.sh" "$BT_FIXTURE/ctl-new"
printf '#!/bin/sh\nexit 1\n' > "$PULSAR_DIR/ctl.sh"
chmod +x "$PULSAR_DIR/ctl.sh"
cat > "$BT_FIXTURE/legacy" <<'EOF'
#!/bin/sh
printf 'legacy-ctl:%s\n' "$PULSAR_BT_MANAGED" >> "$BT_FIXTURE/events"
rm -f "$PULSAR_DIR/enabled"
EOF
chmod +x "$BT_FIXTURE/legacy"
install_fixture "$BT_FIXTURE/legacy"
trace
touch "$PULSAR_DIR/enabled"
supervise
expect 'legacy-ctl:0'
mv "$BT_FIXTURE/ctl-new" "$PULSAR_DIR/ctl.sh"

# Тестовый запуск использует отдельный ctl и снимок. Настоящих SSH и демона здесь нет.
t=$BT_FIXTURE/test
cp /bin/sh "$t/pulsar"
cat > "$PULSAR_DIR/state.conf" <<'EOF'
zigbee_role=pulsar-coordinator
set -eu
printf 'test:%s\n' "$PULSAR_BT_MANAGED" >> "$BT_FIXTURE/events"
if [ "$PULSAR_BT_MANAGED" = 1 ]; then
  [ -d "$PULSAR_BT_PROC/101" ]
  umask 077
  printf '1 0 1\n' > "$PULSAR_BT_OWNER.part"
  mv "$PULSAR_BT_OWNER.part" "$PULSAR_BT_OWNER"
  "$PULSAR_BT_INIT" stop
  [ ! -e "$PULSAR_BT_PROC/101" ]
fi
: > "$BT_FIXTURE/test-ready"
while :; do "$REAL_SLEEP" 0.1; done
EOF
touch "$PULSAR_DIR/zigbee-coordinator.ybl"
trace
touch "$PULSAR_DIR/enabled"
reject sh "$station" lite2 test-start --bt
sh "$station" lite2 test-start
await_test_ready
sh "$station" lite2 test-stop
expect 'test:0'
[ ! -e "$t/bt-owner" ]
grep -qx 'zigbee_role=pulsar-coordinator' "$t/state.conf"
[ -f "$t/zigbee-coordinator.ybl" ]
rm "$t/zigbee-coordinator.ybl"
cp "$PULSAR_DIR/ctl.sh" "$t/ctl.sh"
chmod +x "$t/ctl.sh"
rm "$PULSAR_DIR/enabled" "$t/state.conf"
trace
reject sh "$station" lite2 test-start --bt
[ ! -e "$t/state.conf" ] && [ ! -e "$t/bt-owner" ] && [ ! -e "$t/pid" ] ||
  fail 'Bluetooth test changed state while production was disabled'
touch "$PULSAR_DIR/enabled" "$BT_FIXTURE/quasar-running"
reject sh "$station" lite2 test-start --bt
[ ! -e "$t/state.conf" ] && [ ! -e "$t/bt-owner" ] && [ ! -e "$t/pid" ] ||
  fail 'Bluetooth test changed state while Yandex was running'
rm "$BT_FIXTURE/quasar-running"
chmod 755 "$t"
touch "$BT_FIXTURE/pulsar-running" "$BT_FIXTURE/supervisor-running"
msg=$(sh "$station" lite2 stop 2>&1) && fail 'stop accepted a legacy test directory'
case "$msg" in *'legacy test directory:'*) ;; *) fail "$msg" ;; esac
msg=$(FOREIGN_TEST_DIR=1 sh "$station" lite2 stop 2>&1) && fail 'stop accepted a foreign test directory'
case "$msg" in *'legacy test directory:'*) fail "$msg" ;; esac
mv "$t" "$t.real"
ln -s "$t.real" "$t"
msg=$(sh "$station" lite2 stop 2>&1) && fail 'stop accepted a symlink test directory'
case "$msg" in *'legacy test directory:'*) fail "$msg" ;; esac
rm "$t"
mv "$t.real" "$t"
[ -f "$BT_FIXTURE/pulsar-running" ] && [ -f "$BT_FIXTURE/supervisor-running" ] ||
  fail 'unsafe test directory stranded production'
rm "$BT_FIXTURE/pulsar-running" "$BT_FIXTURE/supervisor-running"
reject sh "$station" lite2 push "$PULSAR_DIR/ctl.sh"
reject sh "$station" lite2 test-start --bt
reject sh "$station" lite2 test-stop
reject sh "$station" lite2 cleanup
[ -x "$t/ctl.sh" ] && [ ! -e "$t/state.conf" ] || fail 'unsafe test directory was modified'
chmod 700 "$t"
expect ''
marker '0 1 0'
trace
sh "$station" lite2 test-start --bt
await_test_ready
[ -f "$t/bt-owner" ] && [ ! -e "$PULSAR_BT_PROC/101" ] || fail 'test did not take Bluetooth ownership'
[ "$(grep '^zigbee_role=' "$t/state.conf")" = 'zigbee_role=off' ]
[ ! -e "$t/zigbee-coordinator.ybl" ]
read -r p < "$t/pid"
test_pid_for_cleanup=$p
: > "$t/pid"
reject sh "$station" lite2 test-stop
[ -f "$t/bt-owner" ] && [ -f "$t/pid" ] || fail 'test-stop ignored a live process with no recorded PID'
kill -KILL "$p"
i=0
while [ "$("$REAL_READLINK" "/proc/$p/exe" 2>/dev/null)" = "$t/pulsar" ]; do
  [ "$i" -lt 100 ] || fail 'test process did not exit after SIGKILL'
  "$REAL_SLEEP" 0.01
  i=$((i + 1))
done
reject env FAIL_START=1 sh "$station" lite2 test-stop
[ -f "$t/bt-owner" ]
reject env FOREIGN_OWNER=1 FOREIGN_MARKER="$t/bt-owner" sh "$station" lite2 test-stop
[ -f "$t/bt-owner" ] || fail 'test-stop consumed a foreign test snapshot'
reject sh "$station" lite2 start
reject sh "$station" lite2 cleanup
sh "$station" lite2 test-stop
[ ! -e "$t/pid" ] || fail 'test-stop left the empty PID file behind'
expect 'test:1
init:stop
Pairable:true
Discoverable:false
init:start
Pairable:true
Discoverable:false
init:start'
[ ! -e "$t/bt-owner" ] && [ "$(cat "$PULSAR_BT_OWNER")" = '0 1 0' ] ||
  fail 'test restore did not isolate the ownership markers'

# Подмена и восстановление установленного бинаря ждут возврата его BT-владения.
cat > "$BT_FIXTURE/swap" <<'EOF'
#!/bin/sh
[ "$*" = --version ]
EOF
cp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/before-swap"
cp "$BT_FIXTURE/stable" "$BT_FIXTURE/before-prev"
cp "$BT_FIXTURE/before-prev" "$PULSAR_DIR/pulsar.prev"
ln -f "$PULSAR_DIR/pulsar" "$PULSAR_DIR/pulsar.bt-blocked"
trace
sh "$station" lite2 stop
expect 'Discoverable:false
Pairable:false'
rm "$PULSAR_BT_OWNER"
touch "$BT_FIXTURE/pulsar-running" "$BT_FIXTURE/supervisor-running"
trace
reject env FAIL_STATION_COPY=1 sh "$station" lite2 swap "$BT_FIXTURE/swap"
expect ''
[ -f "$BT_FIXTURE/pulsar-running" ] && [ -f "$BT_FIXTURE/supervisor-running" ] ||
  fail 'swap stopped the service before staging its replacement'
reject env LATE_ACQUIRE='0 1 1' FAIL_FLAG=Discoverable FAIL_VALUE=true sh "$station" lite2 swap "$BT_FIXTURE/swap"
expect 'init:stop
Discoverable:false
Pairable:false
Pairable:false
Discoverable:true'
cmp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/before-swap"
[ -f "$PULSAR_BT_OWNER" ] && [ ! -e "$BT_FIXTURE/pulsar-running" ] && [ ! -e "$BT_FIXTURE/supervisor-running" ] ||
  fail 'swap did not stop production and retain the late ownership marker'
trace
reject env FAIL_STATION_REPLACE=1 sh "$station" lite2 swap "$BT_FIXTURE/swap"
expect 'Discoverable:false
Pairable:false
Pairable:false
Discoverable:true
init:start
supervisor:start'
cmp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/before-swap"
cmp "$PULSAR_DIR/pulsar.prev" "$BT_FIXTURE/before-prev"
[ "$PULSAR_DIR/pulsar" -ef "$PULSAR_DIR/pulsar.bt-blocked" ]
trace
sh "$station" lite2 swap "$BT_FIXTURE/swap"
expect 'supervisor:start'
cmp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/swap"
touch "$BT_FIXTURE/pulsar-running"
trace
reject env FAIL_STATION_COPY=1 sh "$station" lite2 restore
expect ''
[ -f "$BT_FIXTURE/pulsar-running" ] && [ -f "$BT_FIXTURE/supervisor-running" ] ||
  fail 'restore stopped the service before staging its replacement'
reject env LATE_ACQUIRE='1 0 1' FAIL_FLAG=Pairable FAIL_VALUE=true sh "$station" lite2 restore
expect 'init:stop
Discoverable:false
Pairable:false
Pairable:true'
cmp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/swap"
[ -f "$PULSAR_BT_OWNER" ] && [ -f "$PULSAR_DIR/pulsar.orig" ] ||
  fail 'failed restore discarded the late ownership marker or original backup'
trace
reject env FAIL_STATION_REPLACE=1 sh "$station" lite2 restore
expect 'Discoverable:false
Pairable:false
Pairable:true
Discoverable:false
init:start
supervisor:start'
cmp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/swap"
cmp "$PULSAR_DIR/pulsar.prev" "$BT_FIXTURE/before-swap"
! [ "$PULSAR_DIR/pulsar" -ef "$PULSAR_DIR/pulsar.bt-blocked" ] ||
  fail 'failed restore changed the current Bluetooth guard'
trace
sh "$station" lite2 restore
expect 'supervisor:start'
cmp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/before-swap"
cmp "$PULSAR_DIR/pulsar.prev" "$BT_FIXTURE/before-prev"
[ "$PULSAR_DIR/pulsar" -ef "$PULSAR_DIR/pulsar.bt-blocked" ] ||
  fail 'station restore lost the original Bluetooth guard'
[ ! -e "$PULSAR_BT_OWNER" ] && [ ! -e "$PULSAR_DIR/pulsar.orig" ] ||
  fail 'successful restore left ownership or backup pending'

# With pre-Bluetooth ctl, station handoff still uses the legacy restart.
cat > "$PULSAR_DIR/ctl.sh" <<'EOF'
#!/bin/sh
[ "$*" = restart ] || exit 1
printf 'legacy:restart\n' >> "$BT_FIXTURE/events"
EOF
trace
sh "$station" lite2 swap "$BT_FIXTURE/swap"
sh "$station" lite2 restore
expect 'legacy:restart
legacy:restart'
cmp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/before-swap"

# Новый путь не зависит от коллизии в /tmp и не применяет снимок от прошлой загрузки.
(
  export PULSAR_DIR=$BT_FIXTURE/production PULSAR_BT_OWNER=
  mkdir "$PULSAR_DIR"
  legacy=$BT_FIXTURE/legacy-owner
  for script in ctl.sh supervise.sh; do
    sed "s|/tmp/pulsar-bt-owner|$legacy|g; s|/tmp/pulsar-test|$BT_FIXTURE/test|g" "$here/$script" > "$PULSAR_DIR/$script"
    chmod +x "$PULSAR_DIR/$script"
  done
  owner=$(ctl bt-restore --owner-path)
  case "$owner" in "$PULSAR_DIR"/.pulsar-bt-owner.*) ;; *) fail "unexpected owner path: $owner" ;; esac
  stale=$PULSAR_DIR/.pulsar-bt-owner.previous-boot
  (umask 077; printf '1 0 0\n' > "$legacy"; printf '1 1 0\n' > "$stale")
  cat > "$PULSAR_DIR/pulsar" <<'EOF'
#!/bin/sh
printf 'managed:%s\n' "$PULSAR_BT_MANAGED" >> "$BT_FIXTURE/events"
printf '%s\n' "$PULSAR_BT_OWNER" > "$PULSAR_DIR/owner-path"
rm -f "$PULSAR_DIR/enabled"
EOF
  chmod +x "$PULSAR_DIR/pulsar"
  trace
  touch "$PULSAR_DIR/enabled"
  sh "$PULSAR_DIR/supervise.sh"
  expect 'Pairable:true
Discoverable:false
managed:1'
  [ "$(cat "$PULSAR_DIR/owner-path")" = "$owner" ] && [ ! -e "$legacy" ] && [ ! -e "$stale" ] ||
    fail 'supervisor did not switch to the current boot owner path'
  (umask 077; printf '1 1 0\n' > "$legacy")
  trace
  touch "$PULSAR_DIR/enabled"
  FOREIGN_OWNER=1 FOREIGN_MARKER="$legacy" sh "$PULSAR_DIR/supervise.sh"
  expect 'managed:1'
  [ "$(cat "$legacy")" = '1 1 0' ] || fail 'foreign legacy marker was modified'
  (umask 077; printf '0 1 0\n' > "$owner"; printf '1 0 1\n' > "$stale")
  trace
  FOREIGN_OWNER=1 FOREIGN_MARKER="$legacy" ctl bt-restore
  expect 'Pairable:false
Discoverable:true'
  [ ! -e "$owner" ] && [ -f "$stale" ] ||
    fail 'restore used a snapshot outside this boot'
  rm "$legacy" "$stale"

  # Установленная служба не работает поверх тестового снимка и не вызывает
  # тестовый ctl. Снимок из общего, чужого или подменённого каталога не доверенный.
  (umask 077; printf '1 0 0\n' > "$t/bt-owner"; printf '0 1 0\n' > "$owner")
  printf '#!/bin/sh\necho test-ctl:called >> "$BT_FIXTURE/events"\n' > "$t/ctl.sh"
  touch "$PULSAR_DIR/enabled"
  trace
  for action in enable restart reload rollback disable bt-restore; do reject ctl "$action"; done
  reject ctl bt-restore --hide
  reject "$PULSAR_DIR/supervise.sh"
  mv "$PULSAR_DIR/pulsar" "$PULSAR_DIR/pulsar.saved"
  reject "$PULSAR_DIR/supervise.sh"
  mv "$PULSAR_DIR/pulsar.saved" "$PULSAR_DIR/pulsar"
  expect ''
  [ -f "$PULSAR_DIR/enabled" ] && [ -f "$owner" ] && [ -f "$t/bt-owner" ] ||
    fail 'production mutated state over a pending test snapshot'
  chmod 755 "$t"
  reject env PULSAR_BT_OWNER="$t/bt-owner" "$PULSAR_DIR/ctl.sh" bt-restore
  ctl disable
  expect 'Pairable:false
Discoverable:true
quasar:start'
  chmod 700 "$t"
  trace
  touch "$PULSAR_DIR/enabled"
  FOREIGN_TEST_DIR=1 supervise
  expect 'managed:1'
  mv "$t" "$t.real"
  ln -s "$t.real" "$t"
  trace
  ctl disable
  expect 'quasar:start'
  rm "$t"
  mv "$t.real" "$t"
  [ -f "$t/bt-owner" ] || fail 'production attempted to consume the test snapshot'
  rm "$t/bt-owner"

  # Подменяем рабочий путь после последней сверки inode. Даже первый запуск
  # должен исполнить закреплённый ELF с именем pulsar и рабочим путём обновления.
  install_fixture /bin/sh
  ln "$PULSAR_DIR/pulsar" "$BT_FIXTURE/pre-rename"
  cat > "$PULSAR_DIR/state.conf" <<'EOF'
set -eu
exe=$("$REAL_READLINK" "/proc/$$/exe")
case "$exe" in "$PULSAR_DIR"/.pulsar-bt-executable.*/pulsar) ;; *) exit 2 ;; esac
[ "$PULSAR_BT_MANAGED" = 1 ] && [ "$exe" -ef "$BT_FIXTURE/pre-rename" ] || exit 2
[ "$PULSAR_INSTALLED_BINARY" = "$PULSAR_DIR/pulsar" ] || exit 2
case " $("$REAL_PIDOF" pulsar) " in *" $$ "*) ;; *) exit 2 ;; esac
if [ ! -e "$PULSAR_BT_OWNER" ]; then
  # Без подмены оставляем снимок для следующего запуска.
  (umask 077; printf '1 0 1\n' > "$PULSAR_BT_OWNER")
  "$PULSAR_BT_INIT" stop
  echo pinned:first >> "$BT_FIXTURE/events"
  if [ "$exe" -ef "$PULSAR_INSTALLED_BINARY" ]; then rm "$PULSAR_DIR/enabled"; fi
else
  ! [ "$exe" -ef "$PULSAR_INSTALLED_BINARY" ] || exit 2
  [ ! -d "$PULSAR_BT_PROC/101" ]
  echo pinned:restart >> "$BT_FIXTURE/events"
fi
EOF
  cp "$BT_FIXTURE/stable" "$BT_FIXTURE/late-replacement"
  trace
  touch "$PULSAR_DIR/enabled"
  SHADOW_AT_START=1 supervise
  expect 'install:renamed
init:stop
pinned:first
Discoverable:false
Pairable:false
Pairable:true
Discoverable:false
init:start
stable'
  [ ! -e "$owner" ] || fail 'first launch did not restore before executing its replacement'

  ln -f "$BT_FIXTURE/pre-rename" "$PULSAR_DIR/pulsar"
  trace
  touch "$PULSAR_DIR/enabled"
  supervise
  expect 'init:stop
pinned:first
Discoverable:false
Pairable:false'
  cp "$BT_FIXTURE/stable" "$BT_FIXTURE/late-replacement"
  trace
  touch "$PULSAR_DIR/enabled"
  SHADOW_AT_START=1 supervise
  expect 'install:renamed
pinned:restart
Discoverable:false
Pairable:false
Pairable:true
Discoverable:false
init:start
stable'
  [ ! -e "$owner" ] || fail 'replacement launched before restoring ownership'
)
echo 'Bluetooth ownership tests passed'
