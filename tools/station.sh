#!/bin/sh
# Station helper (docs/stations.md). Never commit station addresses.
# PULSAR_TAG: free-form tag of the work holding the lock (one word, no spaces).
set -eu
usage() {
  echo "usage: PULSAR_TAG=<tag> $0 midi|lite2|mini lock MINUTES|unlock|break-lock|stop|start|push FILE...|test-start [--bt]|test-stop|test-log [N]|swap BINARY|restore|cleanup|status|log [N]" >&2
  exit 2
}
[ $# -ge 2 ] || usage
case "$1" in
  midi) var=PULSAR_MIDI; H=${PULSAR_MIDI-} ;;
  lite2) var=PULSAR_LITE2; H=${PULSAR_LITE2-} ;;
  mini) var=PULSAR_MINI; H=${PULSAR_MINI-} ;;
  *) usage ;;
esac
if [ -z "$H" ]; then
  if [ -n "${XDG_CONFIG_HOME:-${HOME:-}}" ]; then
    file=${XDG_CONFIG_HOME:-$HOME/.config}/pulsar/stations/$1
    if [ -f "$file" ] && [ -r "$file" ]; then
      {
        IFS= read -r H || :
        if IFS= read -r extra || [ -n "$extra" ]; then
          echo "заблокирован: в $file ожидается одна строка с SSH-назначением" >&2
          exit 2
        fi
      } < "$file"
    fi
  fi
  if [ -z "$H" ]; then
    echo "заблокирован: нет $var и локального SSH-назначения для $1" >&2
    exit 2
  fi
fi
case "$H" in
  -*|*[[:space:]]*)
    echo "заблокирован: некорректное SSH-назначение для $1" >&2
    exit 2 ;;
esac
station=$1
cmd=$2
shift 2
L=/tmp/pulsar-agent.lock
T=/tmp/pulsar-test
D=/data/pulsar
BT_OWNER=/tmp/pulsar-bt-owner
r() { ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4 "$H" "$@"; }
# up SRC DEST: upload to DEST.part, verify size and sha256 against SRC, then rename to DEST (mode 755).
up() {
  src=$1 dest=$2
  size=$(wc -c < "$src" | tr -d ' ')
  if command -v sha256sum >/dev/null 2>&1; then sum=$(sha256sum "$src" | cut -d' ' -f1)
  else sum=$(shasum -a 256 "$src" | cut -d' ' -f1); fi
  if ! r "cat > '$dest.part'" < "$src"; then
    r "rm -f '$dest.part'" || true; echo "upload of $src failed" >&2; exit 1
  fi
  got=$(r "s=\$(wc -c < '$dest.part' | tr -d ' '); h=none
    if command -v sha256sum >/dev/null 2>&1; then h=\$(sha256sum '$dest.part' | cut -d' ' -f1); fi
    echo \$s \$h") || got=
  set -- $got
  if [ "${1:-}" != "$size" ] || { [ "${2:-}" != none ] && [ "${2:-}" != "$sum" ]; }; then
    r "rm -f '$dest.part'" || true
    echo "transfer of $src corrupted: local $size bytes sha256 $sum, station ${1:-?} bytes sha256 ${2:-?}" >&2; exit 1
  fi
  [ "${2:-}" != none ] || echo "warning: no sha256sum on station, only size verified for $src" >&2
  r "chmod 755 '$dest.part' && mv '$dest.part' '$dest'"
}
# State-changing commands run only under this tag's own lock.
own() {
  tag=${PULSAR_TAG:?PULSAR_TAG is not set}
  r "read o _ 2>/dev/null < $L/owner && [ \"\$o\" = '$tag' ]" || { echo "no lock held by $tag" >&2; exit 1; }
}
# Remote snippet: remove a stale ALSA dmix lock (shm 0x400/0x401 or the Mini's 0x8e5/0x8e6
# nobody is attached to, then its semaphore) left by a process killed while playing; without it
# the station stays silent until a reboot (satellite/deploy/supervise.sh header). Run where no
# pulsar is playing.
DMIX=$(cat <<'EOF'
r= b=0 s=$(ipcs -m 2>/dev/null)
for k in 400 401 8e5 8e6; do
  n=$(echo "$s" | awk -v k=0x00000$k '$1 == k { print $6; exit }')
  if [ "$n" = 0 ]; then ipcrm -M 0x$k && r="$r shm 0x$k"; elif [ -n "$n" ]; then b=1; fi
done
for k in 400 8e5; do
  if [ $b = 0 ] && ipcs -s 2>/dev/null | awk -v k=0x00000$k '$1 == k { f = 1 } END { exit !f }'; then
    ipcrm -S 0x$k && r="$r sem 0x$k"
  fi
done
if [ -n "$r" ]; then echo "removed stale ALSA dmix lock (${r# })"; fi
EOF
)
# Из /tmp запускаем код только из закрытого каталога текущего SSH-пользователя.
TEST_DIR_CHECK="test_dir_check() {
  [ -d '$T' ] && [ ! -L '$T' ] && uid=\$(id -u) &&
    [ \"\$(stat -c '%u:%a' '$T')\" = \"\$uid:700\" ] ||
    { echo 'unsafe test directory: expected a private directory owned by the SSH user' >&2; return 1; }
}"
TEST_DIR_OPTIONAL="$TEST_DIR_CHECK
  if [ -e '$T' ] || [ -L '$T' ]; then test_dir_check || exit 1; fi"
# Не запускать установленный сервис и не удалять стенд, пока не возвращён тестовый BT.
TEST_BT_RESTORED="$TEST_DIR_OPTIONAL
  if [ -e '$T/bt-owner' ] || [ -L '$T/bt-owner' ]; then echo 'test Bluetooth not restored: run test-stop' >&2; exit 1; fi"
PROD_BT_RESTORE="if [ -e '$BT_OWNER' ] || [ -L '$BT_OWNER' ]; then
  '$D/ctl.sh' reload --stop && '$D/ctl.sh' bt-restore &&
    [ ! -e '$BT_OWNER' ] && [ ! -L '$BT_OWNER' ] ||
    { echo 'installed Bluetooth not restored; leaving binaries untouched' >&2; exit 1; }
fi"
case "$cmd" in
  lock)
    [ $# -eq 1 ] || usage
    tag=${PULSAR_TAG:?PULSAR_TAG is not set}
    case "$tag" in '' | *[[:space:]]*) echo "PULSAR_TAG must be one word" >&2; exit 2 ;; esac
    case "$1" in '' | *[!0-9]*) usage ;; esac
    { [ "$1" -ge 1 ] && [ "$1" -le 180 ]; } || usage
    # The owner file appears complete or not at all (a lock dir without one is a crashed lock).
    r "mkdir $L 2>/dev/null && echo \"$tag \$(date +%s) $1\" > $L/owner.tmp && mv $L/owner.tmp $L/owner" \
      || { echo "station busy: $(r "cat $L/owner; echo now \$(date +%s)")" >&2; exit 1; } ;;
  unlock) own; r "rm -rf $L" ;;
  break-lock)
    r "if [ ! -f $L/owner ]; then [ -d $L ] && echo 'lock without owner' && rm -rf $L
       else read o t m < $L/owner && [ \$(date +%s) -gt \$((t + m * 60 + 1800)) ] && cat $L/owner && rm -rf $L; fi" \
      || { echo "lock missing or not expired" >&2; exit 1; } ;;
  stop)
    own
    r "[ -f $D/enabled ] || { echo 'Yandex mode (no $D/enabled): refusing' >&2; exit 1; }
       killall supervise.sh 2>/dev/null; killall pulsar 2>/dev/null
       i=0; while [ \$i -lt 10 ] && pidof pulsar >/dev/null; do sleep 1; i=\$((i+1)); done
       pidof pulsar >/dev/null && killall -9 pulsar
       i=0; while [ \$i -lt 5 ] && pidof supervise.sh >/dev/null; do sleep 1; i=\$((i+1)); done
       pidof supervise.sh >/dev/null && killall -9 supervise.sh
       sleep 1
       $DMIX
       ! pidof pulsar supervise.sh >/dev/null && echo stopped" ;;
  start) own; r "$TEST_BT_RESTORED; $D/ctl.sh reload" ;;
  push)
    [ $# -ge 1 ] || usage
    own
    r "$TEST_DIR_CHECK
       if [ ! -e '$T' ] && [ ! -L '$T' ]; then (umask 077; mkdir '$T') || exit 1; fi
       test_dir_check"
    for f; do b=$(basename "$f"); up "$f" "$T/$b"; done ;;
  test-start)
    case "$#:${1-}" in
      0:) bt=0 ;;
      1:--bt) bt=1 ;;
      *) usage ;;
    esac
    own
    r "$TEST_DIR_CHECK; test_dir_check"
    # Один уровень для всех моделей: у Мини громкость программная, по той же шкале в дБ.
    vol=0.2
    # A quoted heredoc: no local shell expansion and no nested quotes in the remote command.
    r sh -s -- "$T" "$D" "$vol" "$station" "$bt" <<'EOF'
T=$1
D=$2
vol=$3
station=$4
bt=$5
! pidof supervise.sh >/dev/null || { echo 'run stop first' >&2; exit 1; }
! pidof pulsar >/dev/null || { echo 'pulsar still running: run stop or test-stop first' >&2; exit 1; }
[ ! -e "$T/bt-owner" ] && [ ! -L "$T/bt-owner" ] ||
  { echo 'test Bluetooth not restored: run test-stop' >&2; exit 1; }
[ -x "$T/pulsar" ] || { echo 'push pulsar first' >&2; exit 1; }
if [ "$bt" = 1 ]; then
  [ -f "$D/enabled" ] && ! pidof quasar_launcher2 maind >/dev/null ||
    { echo 'Yandex mode or stack running: Bluetooth test refused' >&2; exit 1; }
  if ! "$T/ctl.sh" bt-restore --check >/dev/null 2>&1; then
    echo 'Bluetooth test needs a separately pushed satellite/deploy/ctl.sh with bt-restore' >&2
    exit 1
  fi
fi
# api_key не копируется: тестовый экземпляр отдаёт plaintext API, ключ клиенту не нужен.
if [ "$bt" = 1 ]; then set -- -e '^zigbee_role='; else set --; fi
grep -v -e '^volume=' -e '^volume2=' -e '^volume_curve=' -e '^muted=' -e '^auto_update=' -e '^update_attempt=' \
  "$@" "$D/state.conf" > "$T/state.conf" 2>/dev/null || true
printf 'volume2=%s\nmuted=0\nauto_update=0\n' "$vol" >> "$T/state.conf" || exit 1
if [ "$bt" = 1 ]; then
  # Миди не должна открывать радио или применять его образ в Bluetooth-тесте.
  printf 'zigbee_role=off\n' >> "$T/state.conf" || exit 1
else
  for z in "$D"/zigbee-*.ybl; do [ -f "$z" ] && cp "$z" "$T/"; done
fi
cd "$T" || exit 1
if [ "$station" = mini ]; then
  [ -f "$D/asound.conf" ] || { echo 'Mini needs /data/pulsar/asound.conf (satellite/deploy/asound.conf)' >&2; exit 1; }
fi
: > "$T/pid" || exit 1
nohup env PULSAR_PORT_OFFSET=10000 PULSAR_BT_MANAGED="$bt" PULSAR_BT_OWNER="$T/bt-owner" \
  ./pulsar "$T/state.conf" < /dev/null >> "$T/log" 2>&1 &
p=$!
if ! printf '%s\n' "$p" > "$T/pid"; then
  kill "$p" 2>/dev/null
  echo 'could not record test pid: check test-log and run test-stop' >&2
  exit 1
fi
sleep 2
kill -0 "$p" && echo started ||
  { echo 'test pulsar exited: check test-log and run test-stop' >&2; exit 1; }
EOF
    ;;
  test-stop)
    own
    r "$TEST_DIR_OPTIONAL"
    r sh -s -- "$T" <<'EOF'
T=$1
test_running() {
  case "$(readlink "/proc/$p/exe" 2>/dev/null)" in
    "$T/pulsar"|"$T/pulsar (deleted)") return 0 ;;
    *) return 1 ;;
  esac
}

if [ -f "$T/pid" ]; then
  IFS= read -r p < "$T/pid" || exit 1
  case "$p" in
    ''|*[!0-9]*) echo 'invalid test pid file' >&2; exit 1 ;;
  esac
  [ "$p" -gt 1 ] || exit 1
  if test_running; then
    kill "$p" 2>/dev/null
    i=0
    while [ "$i" -lt 10 ] && test_running; do sleep 1; i=$((i + 1)); done
    if test_running; then
      kill -KILL "$p" 2>/dev/null
      i=0
      while [ "$i" -lt 5 ] && test_running; do sleep 1; i=$((i + 1)); done
    fi
    ! test_running || { echo 'test pulsar still running, Bluetooth not restored' >&2; exit 1; }
  fi
  rm -f "$T/pid" || exit 1
fi
if [ -e "$T/bt-owner" ] || [ -L "$T/bt-owner" ]; then
  if ! PULSAR_BT_OWNER="$T/bt-owner" "$T/ctl.sh" bt-restore; then
    echo 'test Bluetooth not restored: keep the test files, do not start the installed service' >&2
    exit 1
  fi
fi
EOF
    r "$DMIX"
    echo stopped ;;
  test-log) r "$TEST_DIR_CHECK; test_dir_check && tail -n ${1:-50} $T/log" ;;
  swap)
    [ $# -eq 1 ] || usage
    own
    # The installed binary is kept as pulsar.orig and serves as .prev meanwhile, so a rollback by
    # supervise.sh lands on it; the owner's .prev waits in pulsar.prev.orig.
    # df -P: busybox `df -k` переносит строку при длинном имени устройства.
    r "$TEST_BT_RESTORED
       [ -f $D/enabled ] || { echo 'Yandex mode (no $D/enabled): refusing' >&2; exit 1; }
       [ \$(df -Pk $D | awk 'NR==2 {print \$4}') -gt 30000 ] || { echo 'less than 30 MB free in $D' >&2; exit 1; }
       cd $D && { [ -f pulsar.orig ] || cp -p pulsar pulsar.orig; } \
         && { [ ! -f pulsar.prev ] || [ -f pulsar.prev.orig ] || cp -p pulsar.prev pulsar.prev.orig; }"
    up "$1" "$D/pulsar.new"
    r "$TEST_BT_RESTORED
       cd $D && ./pulsar.new --version || exit 1
       $PROD_BT_RESTORE
       cp -p pulsar.orig pulsar.prev && mv pulsar.new pulsar && sync && ./ctl.sh restart" ;;
  restore)
    own
    # Если после краш-цикла supervise.sh удалил enabled (give_back), станцию возвращает ctl.sh enable.
    r "$TEST_BT_RESTORED
       cd $D && [ -f pulsar.orig ] || { echo 'no pulsar.orig' >&2; exit 1; }
       $PROD_BT_RESTORE
       cd $D && cp -p pulsar.orig pulsar.tmp && mv pulsar.tmp pulsar && rm -f pulsar.orig || exit 1
       if [ -f pulsar.prev.orig ]; then mv pulsar.prev.orig pulsar.prev; fi
       sync
       if [ -f enabled ]; then ./ctl.sh restart; else ./ctl.sh enable; fi" ;;
  cleanup)
    own
    r "$TEST_BT_RESTORED
       if [ -f $T/pid ] && kill -0 \$(cat $T/pid) 2>/dev/null; then echo 'test instance running: test-stop first' >&2; exit 1; fi
       rm -rf $T || exit 1
       $DMIX" ;;
  status) r "$D/ctl.sh status; ls -l $D; df -Pk $D; cat $L/owner 2>/dev/null || echo 'no lock'" ;;
  log) r "tail -n ${1:-50} /tmp/pulsar.log" ;;
  *) usage ;;
esac
