#!/bin/sh
# Запуск из любого каталога: sh satellite/deploy/test-bt-ownership.sh
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
station=$here/../../tools/station.sh
BT_FIXTURE=$(mktemp -d)
cleanup() {
  if [ -s "$BT_FIXTURE/test/pid" ]; then
    read -r p < "$BT_FIXTURE/test/pid"
    [ "$("$REAL_READLINK" "/proc/$p/exe" 2>/dev/null)" != "$BT_FIXTURE/test/pulsar" ] || kill -KILL "$p"
  fi
  rm -rf "$BT_FIXTURE"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
REAL_READLINK=$(command -v readlink)
REAL_SLEEP=$(command -v sleep)
REAL_MV=$(command -v mv)
export BT_FIXTURE REAL_READLINK REAL_SLEEP REAL_MV
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
cat > "$BT_FIXTURE/bin/dbus-send" <<'EOF'
#!/bin/sh
set -eu
[ "$*" = "--system --print-reply --reply-timeout=2000 --dest=org.bluez /org/bluez/hci0 org.freedesktop.DBus.Properties.Set string:org.bluez.Adapter1 $8 $9" ]
prop=${8#string:}
printf '%s:%s\n' "$prop" "${9#variant:boolean:}" >> "$BT_FIXTURE/events"
[ "$prop" != "${FAIL_FLAG-}" ]
EOF
cat > "$PULSAR_BT_INIT" <<'EOF'
#!/bin/sh
set -eu
printf 'init:%s\n' "$1" >> "$BT_FIXTURE/events"
p=$PULSAR_BT_PROC/101
case "$1" in
  stop) [ "${FAIL_STOP:-0}" = 0 ] && rm -rf "$p" ;;
  start)
    [ "${FAIL_START:-0}" = 0 ] || exit 1
    [ "${NO_SPAWN:-0}" = 0 ] || exit 0
    mkdir -p "$p"
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
    if [ -n "${SELF_PID-}" ]; then echo "$SELF_PID"; exit 0; fi
    if [ -f "$BT_FIXTURE/pulsar-running" ]; then echo 501; exit 0; fi
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
  pulsar|'-9 pulsar') rm -f "$BT_FIXTURE/pulsar-running" ;;
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
# Ротация логов не должна крутиться без пауз. Дать тестовому процессу время запуститься.
case "$1" in 60|2) exec "$REAL_SLEEP" 0.15 ;; 1) exec "$REAL_SLEEP" 0.01 ;; esac
EOF
printf '#!/bin/sh\nexit 1\n' > "$BT_FIXTURE/bin/ipcs"
cat > "$BT_FIXTURE/bin/setsid" <<'EOF'
#!/bin/sh
[ "$*" = "$PULSAR_DIR/supervise.sh" ] || exit 2
echo 'supervisor:start' >> "$BT_FIXTURE/events"
touch "$BT_FIXTURE/supervisor-running"
EOF
cat > "$BT_FIXTURE/bin/mv" <<'EOF'
#!/bin/sh
if [ "${FAIL_REPLACE:-0}" = 1 ] && [ "$*" = '-f pulsar.rb pulsar' ]; then exit 1; fi
exec "$REAL_MV" "$@"
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
cp "$here/ctl.sh" "$here/supervise.sh" "$PULSAR_DIR/"
chmod +x "$PULSAR_DIR/ctl.sh" "$PULSAR_DIR/supervise.sh"
ctl() { "$PULSAR_DIR/ctl.sh" "$@"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
reject() { if "$@"; then fail "expected failure: $*"; fi; }
expect() { [ "$(cat "$events")" = "$1" ] || fail "expected [$1]; got [$(cat "$events")]"; }
marker() { (umask 077; printf '%s\n' "$1" > "$PULSAR_BT_OWNER"); }
trace() { : > "$events"; }

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
ctl bt-restore --check
ctl bt-restore
expect ''
marker '1 0 1'
chmod 644 "$PULSAR_BT_OWNER"
reject ctl bt-restore
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
touch "$BT_FIXTURE/pulsar-running"
reject ctl bt-restore --self
rm "$BT_FIXTURE/pulsar-running"
sh -c 'SELF_PID=$$ "$1" bt-restore --self; rc=$?; exit "$rc"' fixture "$PULSAR_DIR/ctl.sh"
expect 'Pairable:false
Discoverable:true'
trace

# Обычный рестарт не применяет снимок. Disable возвращает только исходные флаги/процесс.
marker '0 1 0'
touch "$PULSAR_DIR/enabled" "$BT_FIXTURE/pulsar-running" "$BT_FIXTURE/supervisor-running"
ctl restart
expect ''
[ -f "$PULSAR_BT_OWNER" ]
touch "$BT_FIXTURE/pulsar-running"
ctl reload --stop
expect ''
[ -f "$PULSAR_BT_OWNER" ]
[ -f "$PULSAR_DIR/enabled" ]
touch "$BT_FIXTURE/pulsar-running"
ctl disable
expect 'Pairable:false
Discoverable:true
quasar:start'
[ ! -e "$PULSAR_BT_OWNER" ] && [ ! -e "$PULSAR_DIR/enabled" ] || fail 'disable left ownership pending'

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

# Совпадающий comm без совпадающего /proc/PID/exe не является штатным процессом.
mkdir -p "$PULSAR_BT_PROC/102"
printf '%s\n' bluealsa-aplay > "$PULSAR_BT_PROC/102/comm"
ln -s /elsewhere/bluealsa-aplay "$PULSAR_BT_PROC/102/exe"
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
[ -d "$PULSAR_BT_PROC/101" ] && [ -d "$PULSAR_BT_PROC/102" ] && [ ! -e "$PULSAR_BT_OWNER" ] ||
  fail 'stock process not restored or unrelated process affected'

# Краш-цикл меняет бинарь только после восстановления, не возвращая player между попытками.
cat > "$PULSAR_DIR/pulsar" <<'EOF'
#!/bin/sh
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
"$PULSAR_BT_INIT" stop
marker '1 1 1'
trace
touch "$PULSAR_DIR/enabled"
reject env FAIL_START=1 sh "$here/supervise.sh"
expect "$loops
Pairable:true
Discoverable:true
init:start"
[ -f "$PULSAR_BT_OWNER" ] && cmp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/new" || fail 'rollback ran after failed restore'
trace
sh "$here/supervise.sh"
expect "$loops
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
[ -f "$BT_FIXTURE/pulsar-running" ] && [ -f "$BT_FIXTURE/supervisor-running" ]
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
rm "$BT_FIXTURE/supervisor-running"
trace
sh "$here/supervise.sh"
expect 'manual-prev:0'
cp "$BT_FIXTURE/before-manual" "$PULSAR_DIR/pulsar.new"
mv "$PULSAR_DIR/pulsar.new" "$PULSAR_DIR/pulsar"
trace
touch "$PULSAR_DIR/enabled"
PULSAR_BT_MANAGED=0 sh "$here/supervise.sh"
expect 'corrected:0'

# Ветка give_back без пригодной .prev использует тот же протокол.
rm -f "$PULSAR_DIR/pulsar" "$PULSAR_DIR/pulsar.prev"
marker '0 0 0'
trace
touch "$PULSAR_DIR/enabled"
reject sh "$here/supervise.sh"
expect 'Pairable:false
Discoverable:false
quasar:start'
[ ! -e "$PULSAR_BT_OWNER" ] && [ ! -e "$PULSAR_DIR/enabled" ] || fail 'give_back left ownership pending'

# Новый supervisor со старым ctl не даёт демону захватывать проигрыватель.
mv "$PULSAR_DIR/ctl.sh" "$BT_FIXTURE/ctl-new"
printf '#!/bin/sh\nexit 1\n' > "$PULSAR_DIR/ctl.sh"
chmod +x "$PULSAR_DIR/ctl.sh"
cat > "$PULSAR_DIR/pulsar" <<'EOF'
#!/bin/sh
printf 'legacy-ctl:%s\n' "$PULSAR_BT_MANAGED" >> "$BT_FIXTURE/events"
rm -f "$PULSAR_DIR/enabled"
EOF
chmod +x "$PULSAR_DIR/pulsar"
trace
touch "$PULSAR_DIR/enabled"
sh "$here/supervise.sh"
expect 'legacy-ctl:0'
mv "$BT_FIXTURE/ctl-new" "$PULSAR_DIR/ctl.sh"

# Тестовый запуск использует отдельный ctl и снимок. Настоящих SSH и демона здесь нет.
t=$BT_FIXTURE/test
cp /bin/sh "$t/pulsar"
cat > "$PULSAR_DIR/state.conf" <<'EOF'
zigbee_role=pulsar-coordinator
printf 'test:%s\n' "$PULSAR_BT_MANAGED" >> "$BT_FIXTURE/events"
if [ "$PULSAR_BT_MANAGED" = 1 ]; then
  umask 077
  printf '1 0 1\n' > "$PULSAR_BT_OWNER.part"
  mv "$PULSAR_BT_OWNER.part" "$PULSAR_BT_OWNER"
  "$PULSAR_BT_INIT" stop
fi
while :; do "$REAL_SLEEP" 0.1; done
EOF
touch "$PULSAR_DIR/zigbee-coordinator.ybl"
trace
touch "$PULSAR_DIR/enabled"
reject sh "$station" lite2 test-start --bt
sh "$station" lite2 test-start
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
[ ! -e "$t/state.conf" ] && [ ! -e "$t/bt-owner" ] && [ ! -e "$t/pid" ]
touch "$PULSAR_DIR/enabled" "$BT_FIXTURE/quasar-running"
reject sh "$station" lite2 test-start --bt
[ ! -e "$t/state.conf" ] && [ ! -e "$t/bt-owner" ] && [ ! -e "$t/pid" ]
rm "$BT_FIXTURE/quasar-running"
chmod 755 "$t"
reject sh "$station" lite2 push "$PULSAR_DIR/ctl.sh"
reject sh "$station" lite2 test-start --bt
reject sh "$station" lite2 test-stop
reject sh "$station" lite2 cleanup
[ -x "$t/ctl.sh" ] && [ ! -e "$t/state.conf" ]
chmod 700 "$t"
expect ''
marker '0 1 0'
trace
sh "$station" lite2 test-start --bt
[ "$(grep '^zigbee_role=' "$t/state.conf")" = 'zigbee_role=off' ]
[ ! -e "$t/zigbee-coordinator.ybl" ]
read -r p < "$t/pid"
kill -KILL "$p"
reject env FAIL_START=1 sh "$station" lite2 test-stop
[ -f "$t/bt-owner" ]
reject sh "$station" lite2 start
reject sh "$station" lite2 cleanup
sh "$station" lite2 test-stop
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
touch "$BT_FIXTURE/pulsar-running" "$BT_FIXTURE/supervisor-running"
trace
reject env FAIL_FLAG=Pairable sh "$station" lite2 swap "$BT_FIXTURE/swap"
expect 'Pairable:false'
cmp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/before-swap"
[ -f "$PULSAR_BT_OWNER" ] && [ ! -e "$BT_FIXTURE/pulsar-running" ] && [ ! -e "$BT_FIXTURE/supervisor-running" ]
trace
sh "$station" lite2 swap "$BT_FIXTURE/swap"
expect 'Pairable:false
Discoverable:true
supervisor:start'
cmp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/swap"
marker '1 0 0'
touch "$BT_FIXTURE/pulsar-running"
trace
reject env FAIL_FLAG=Discoverable sh "$station" lite2 restore
expect 'Pairable:true
Discoverable:false'
cmp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/swap"
[ -f "$PULSAR_BT_OWNER" ] && [ -f "$PULSAR_DIR/pulsar.orig" ]
trace
sh "$station" lite2 restore
expect 'Pairable:true
Discoverable:false
supervisor:start'
cmp "$PULSAR_DIR/pulsar" "$BT_FIXTURE/before-swap"
[ ! -e "$PULSAR_BT_OWNER" ] && [ ! -e "$PULSAR_DIR/pulsar.orig" ]
echo 'Bluetooth ownership tests passed'
