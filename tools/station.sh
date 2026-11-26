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
# Имя файла передаётся в команду SSH, содержимое — без преобразований через stdin.
shell_quote() {
  word=$1
  printf "'"
  while case "$word" in *"'"*) true ;; *) false ;; esac; do
    printf '%s' "${word%%\'*}" "'\\''"
    word=${word#*\'}
  done
  printf "%s'" "$word"
}
up() {
  src=$1 dir=$2 name=$3
  prepare= check=
  if [ "$dir" = "$T" ]; then
    prepare="$TEST_LOCK
      test_lifecycle_lock '$D'
      $TEST_DIR_CHECK
      if [ ! -e \"\$dir\" ] && [ ! -L \"\$dir\" ]; then (umask 077; mkdir \"\$dir\") || exit 1; fi
      test_dir_check \"\$dir\" || exit 1"
    check='test_dir_check . || exit 1
      parent=$(cd -P "${dir%/*}" && pwd -P) || exit 1
      [ "$(pwd -P)" = "$parent/${dir##*/}" ] || exit 1'
  fi
  size=$(wc -c < "$src") || return 1
  if command -v sha256sum >/dev/null 2>&1; then hash=$(sha256sum < "$src") || return 1
  else hash=$(shasum -a 256 < "$src") || return 1; fi
  hash=${hash%% *}
  r "dir='$dir'
    $prepare
    cd -P \"\$dir\" || exit 1
    $check
    dest=$(shell_quote "./$name")
    [ ! -d \"\$dest\" ] || exit 1
    tmp=\$(mktemp ./.upload.XXXXXX) || exit 1
    trap 'rm -f \"\$tmp\"' EXIT
    cat > \"\$tmp\" || exit 1
    got=\$(wc -c < \"\$tmp\") || exit 1
    if [ \"\$got\" -ne '$size' ]; then
      echo \"transfer corrupted: expected $size bytes, got \$got\" >&2
      exit 1
    fi
    if command -v sha256sum >/dev/null 2>&1; then
      got=\$(sha256sum < \"\$tmp\") || exit 1
      if [ \"\${got%% *}\" != '$hash' ]; then
        echo 'transfer corrupted: SHA-256 mismatch' >&2
        exit 1
      fi
    else
      echo 'warning: no sha256sum on station, only size verified' >&2
    fi
    chmod 755 \"\$tmp\" && mv -f \"\$tmp\" \"\$dest\"" < "$src"
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
TEST_DIR_CHECK=$(cat <<'EOF'
test_dir_check() {
  if [ -d "$1" ] && [ ! -L "$1" ] && uid=$(id -u); then
    case $(stat -c '%u:%a' "$1") in
      "$uid:700") return 0 ;;
      "$uid:755")
        echo "legacy test directory: if no test or Bluetooth recovery is pending, move $1 to an unused name, keep it for inspection, and push fresh files" >&2 ;;
    esac
  fi
  echo 'unsafe test directory: expected a private directory owned by the SSH user' >&2
  return 1
}
EOF
)
TEST_DIR_OPTIONAL="$TEST_DIR_CHECK
  if [ -e '$T' ] || [ -L '$T' ]; then test_dir_check '$T' || exit 1; fi"
# Один inode на весь жизненный цикл стенда. cleanup не удаляет его вместе с тестовыми файлами.
# flock доступен на станциях и сам отпускает блокировку при гибели последнего владельца FD.
TEST_LOCK=$(cat <<'EOF'
test_lifecycle_lock() {
  exec 9>> "$1/.station-test.lock" || exit 1
  flock -n 9 || { echo 'test lifecycle lock unavailable: retry after the other command ends' >&2; exit 1; }
}
# Обёртка удерживает блокировку до возврата ctl, не передавая её демонам.
test_lifecycle_child() (
  "$@" 9>&-
  exit $?
)
EOF
)
# Не запускать установленный сервис и не удалять стенд, пока не возвращён тестовый BT.
TEST_BT_RESTORED="$TEST_DIR_OPTIONAL
  if [ -e '$T/bt-reserved' ] || [ -L '$T/bt-reserved' ] ||
    [ -e '$T/bt-owner' ] || [ -L '$T/bt-owner' ] ||
    [ -e '$T/bt-owner.pending' ] || [ -L '$T/bt-owner.pending' ]; then
    echo 'test Bluetooth not restored: run test-stop' >&2
    exit 1
  fi"
TEST_HANDOFF="$TEST_LOCK
  test_lifecycle_lock '$D'
  $TEST_BT_RESTORED"
# Сначала остановить обоих: работающий pulsar ещё может записать снимок и захватить player.
PROD_BT_CTL="'$D/ctl.sh' bt-restore --supported >/dev/null 2>&1 ||
  '$D/ctl.sh' bt-restore --check >/dev/null 2>&1"
PROD_BT_RESTORE="boot=\$(cat /proc/sys/kernel/random/boot_id 2>/dev/null)
if [ -n \"\$boot\" ] &&
  { [ -e '$D'/.pulsar-bt-owner.\"\$boot\" ] || [ -L '$D'/.pulsar-bt-owner.\"\$boot\" ] ||
    [ -e '$D'/.pulsar-bt-owner.\"\$boot\".pending ] || [ -L '$D'/.pulsar-bt-owner.\"\$boot\".pending ]; } &&
  ! '$D/ctl.sh' bt-restore --owner-path >/dev/null 2>&1; then
  echo 'installed ctl.sh cannot recover the current Bluetooth snapshot' >&2
  exit 1
fi
if $PROD_BT_CTL; then
  '$D/ctl.sh' reload --stop && test_lifecycle_child '$D/ctl.sh' bt-restore ||
    { echo 'installed Bluetooth not restored; leaving binaries untouched' >&2; exit 1; }
else
  # Старый ctl не может восстановить снимок от ранее установленного бинаря.
  for marker in '$BT_OWNER' '$BT_OWNER.pending'; do
    [ -e \"\$marker\" ] || [ -L \"\$marker\" ] || continue
    owner=\$(stat -c '%u' \"\$marker\") && uid=\$(id -u) && [ \"\$owner\" != \"\$uid\" ] ||
      { echo 'installed Bluetooth not restored; leaving binaries untouched' >&2; exit 1; }
  done
fi"
# Готовим файлы до остановки сервиса. Сохраняем прежний слот отката и защиту
# по inode, пока не обновлены метаданные и не переименован исполняемый файл.
PROD_REPLACE="stage_replace() {
  stage=\$(mktemp -d .station-replace.XXXXXX) || exit 1
  trap 'rm -rf -- \"\$stage\"' EXIT
  [ ! -e pulsar.prev ] || ln pulsar.prev \"\$stage/prev.old\" || exit 1
}
stage_guard() {
  [ -f \"\$stage/guard\" ] || return 0
  if [ pulsar -ef pulsar.bt-blocked.next ]; then
    mv -f pulsar.bt-blocked.next pulsar.bt-blocked || return 1
  fi
  mv -f \"\$stage/guard\" pulsar.bt-blocked.next
}
replace_pulsar() {
  prev_changed=0
  if { if [ -f \"\$stage/prev\" ]; then mv -f \"\$stage/prev\" pulsar.prev; else rm -f pulsar.prev; fi; } &&
     { prev_changed=1; stage_guard; } &&
     mv -f \"\$1\" pulsar; then
    if [ pulsar -ef pulsar.bt-blocked.next ]; then
      mv -f pulsar.bt-blocked.next pulsar.bt-blocked
    fi
    return 0
  fi
  echo 'replacement failed; keeping the installed binary' >&2
  if ! [ pulsar -ef pulsar.bt-blocked.next ]; then rm -f pulsar.bt-blocked.next; fi
  if [ \"\$prev_changed\" = 1 ]; then
    if [ -f \"\$stage/prev.old\" ]; then mv -f \"\$stage/prev.old\" pulsar.prev
    else rm -f pulsar.prev; fi || echo 'could not restore the previous rollback binary' >&2
  fi
  [ ! -f enabled ] || test_lifecycle_child ./ctl.sh restart
  exit 1
}"
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
       $TEST_BT_RESTORED
       if $PROD_BT_CTL; then
         '$D/ctl.sh' reload --stop || exit 1
       else
         killall supervise.sh 2>/dev/null; killall pulsar 2>/dev/null
         i=0; while [ \$i -lt 10 ] && pidof pulsar >/dev/null; do sleep 1; i=\$((i+1)); done
         pidof pulsar >/dev/null && killall -9 pulsar
         i=0; while [ \$i -lt 5 ] && pidof supervise.sh >/dev/null; do sleep 1; i=\$((i+1)); done
         pidof supervise.sh >/dev/null && killall -9 supervise.sh
         sleep 1
       fi
       $DMIX
       ! pidof pulsar supervise.sh >/dev/null && echo stopped" ;;
  start)
    own
    r "$TEST_HANDOFF
       test_lifecycle_child '$D/ctl.sh' reload" ;;
  push)
    [ $# -ge 1 ] || usage
    own
    for f; do up "$f" "$T" "${f##*/}"; done ;;
  test-start)
    case "$#:${1-}" in
      0:) bt=0 ;;
      1:--bt) bt=1 ;;
      *) usage ;;
    esac
    own
    # Один уровень для всех моделей: у Мини громкость программная, по той же шкале в дБ.
    vol=0.2
    # A quoted heredoc: no local shell expansion and no nested quotes in the remote command.
    {
      printf '%s\n' "$TEST_LOCK" "$TEST_DIR_CHECK"
      cat <<'EOF'
T=$1
D=$2
vol=$3
station=$4
bt=$5
test_lifecycle_lock "$D"
test_dir_check "$T" || exit 1
production_stopped() {
  ! pidof supervise.sh >/dev/null || { echo 'run stop first' >&2; return 1; }
  ! pidof pulsar >/dev/null || { echo 'pulsar still running: run stop or test-stop first' >&2; return 1; }
}
production_stopped || exit 1
[ ! -e "$T/bt-reserved" ] && [ ! -L "$T/bt-reserved" ] &&
  [ ! -e "$T/bt-owner" ] && [ ! -L "$T/bt-owner" ] &&
  [ ! -e "$T/bt-owner.pending" ] && [ ! -L "$T/bt-owner.pending" ] ||
  { echo 'test Bluetooth not restored: run test-stop' >&2; exit 1; }
[ -x "$T/pulsar" ] || { echo 'push pulsar first' >&2; exit 1; }
if [ "$bt" = 1 ]; then
  stock_stopped() {
    [ -f "$D/enabled" ] && ! pidof quasar_launcher2 maind >/dev/null ||
      { echo 'Yandex mode or stack running: Bluetooth test refused' >&2; return 1; }
  }
  stock_stopped || exit 1
  if ! "$T/ctl.sh" bt-restore --test-check >/dev/null 2>&1; then
    echo 'Bluetooth test needs a separately pushed satellite/deploy/ctl.sh with bt-restore' >&2
    exit 1
  fi
  # Одним ln публикуем резерв и PID начала запуска. test-stop не снимает
  # резерв, пока этот shell может ещё запустить процесс.
  started=$(awk '{ sub(/^.*\) /, ""); print $20 }' "/proc/$$/stat") || exit 1
  [ -n "$started" ] || exit 1
  ln -s "$$:$started" "$T/bt-reserved" ||
    { echo 'test Bluetooth already reserved: run test-stop' >&2; exit 1; }
  trap 'rm -f "$T/bt-reserved"' EXIT
  # Supervisor, появившийся до резерва, побеждает; появившийся после
  # увидит резерв сам. Не изменяем тестовый state.conf при отказе.
  production_stopped && stock_stopped || exit 1
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
# После этой точки резерв снимает только test-stop, даже если запуск оборвётся.
trap '' HUP
trap - EXIT
nohup env PULSAR_PORT_OFFSET=10000 PULSAR_BT_MANAGED="$bt" PULSAR_BT_OWNER="$T/bt-owner" \
  ./pulsar "$T/state.conf" 9>&- < /dev/null >> "$T/log" 2>&1 &
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
    } | r sh -s -- "$T" "$D" "$vol" "$station" "$bt"
    ;;
  test-stop)
    own
    {
      printf '%s\n' "$TEST_LOCK" "$TEST_DIR_CHECK"
      cat <<'EOF'
T=$1
test_lifecycle_lock "$2"
if [ -e "$T" ] || [ -L "$T" ]; then test_dir_check "$T" || exit 1; fi
test_bt_pending() {
  [ -e "$T/bt-owner" ] || [ -L "$T/bt-owner" ] ||
    [ -e "$T/bt-owner.pending" ] || [ -L "$T/bt-owner.pending" ]
}
reserved=0
if [ -e "$T/bt-reserved" ] || [ -L "$T/bt-reserved" ]; then reserved=1; fi
if [ -L "$T/bt-reserved" ]; then
  starter=$(readlink "$T/bt-reserved") || exit 1
  owner=${starter%%:*}
  case "$owner" in ''|*[!0-9]*) ;; *)
    if [ "$starter" = "$owner:$(awk '{ sub(/^.*\) /, ""); print $20 }' "/proc/$owner/stat" 2>/dev/null)" ]; then
      echo 'test-start is still launching the daemon' >&2
      exit 1
    fi ;;
  esac
fi
test_running() {
  case "$(readlink "/proc/$p/exe" 2>/dev/null)" in
    "$T/pulsar"|"$T/pulsar (deleted)") return 0 ;;
    *) return 1 ;;
  esac
}

if [ -s "$T/pid" ]; then
  if ! IFS= read -r p < "$T/pid"; then p=; fi
  case "$p" in ''|*[!0-9]*) p=0 ;; esac
  if [ "$p" -gt 1 ] 2>/dev/null && test_running; then
    kill "$p" 2>/dev/null
    i=0
    while [ "$i" -lt 10 ] && test_running; do sleep 1; i=$((i + 1)); done
    if test_running; then
      kill -KILL "$p" 2>/dev/null
      i=0
      while [ "$i" -lt 5 ] && test_running; do sleep 1; i=$((i + 1)); done
    fi
  fi
fi
if [ -e "$T/pid" ] || [ "$reserved" = 1 ] || test_bt_pending; then
  for exe in /proc/[0-9]*/exe; do
    p=${exe%/exe}
    p=${p##*/}
    ! test_running || { echo 'test pulsar still running, Bluetooth not restored' >&2; exit 1; }
  done
  rm -f "$T/pid" || exit 1
fi
if test_bt_pending; then
  if ! test_lifecycle_child "$T/ctl.sh" bt-restore --test; then
    echo 'test Bluetooth not restored: keep the test files, do not start the installed service' >&2
    exit 1
  fi
  if test_bt_pending; then
    echo 'test Bluetooth not restored: push the current satellite/deploy/ctl.sh and retry test-stop; do not start the installed service' >&2
    exit 1
  fi
fi
EOF
      printf '%s\n' "$DMIX" '[ "$reserved" = 0 ] || rm -f "$T/bt-reserved"'
    } | r sh -s -- "$T" "$D"
    echo stopped ;;
  test-log) r "$TEST_DIR_CHECK; test_dir_check '$T' && tail -n ${1:-50} $T/log" ;;
  swap)
    [ $# -eq 1 ] || usage
    own
    # The installed binary is kept as pulsar.orig and serves as .prev meanwhile, so a rollback by
    # supervise.sh lands on it; the owner's .prev waits in pulsar.prev.orig.
    # df -P: busybox `df -k` переносит строку при длинном имени устройства.
    r "$TEST_HANDOFF
       [ -f $D/enabled ] || { echo 'Yandex mode (no $D/enabled): refusing' >&2; exit 1; }
       [ \$(df -Pk $D | awk 'NR==2 {print \$4}') -gt 30000 ] || { echo 'less than 30 MB free in $D' >&2; exit 1; }
       cd $D || exit 1
       if [ ! -f pulsar.orig ]; then
         $PROD_REPLACE
         stage_replace
         ln pulsar \"\$stage/orig\" && rm -f pulsar.orig.bt-blocked pulsar.prev.orig || exit 1
         if [ \"\$stage/orig\" -ef pulsar.bt-blocked ] || [ \"\$stage/orig\" -ef pulsar.bt-blocked.next ]; then
           ln \"\$stage/orig\" pulsar.orig.bt-blocked || exit 1
         fi
         if [ -f \"\$stage/prev.old\" ]; then ln \"\$stage/prev.old\" pulsar.prev.orig || exit 1; fi
         mv \"\$stage/orig\" pulsar.orig
       fi"
    up "$1" "$D" pulsar.new
    r "$TEST_HANDOFF
       cd $D && ./pulsar.new --version || exit 1
       $PROD_REPLACE
       stage_replace
       cp -p pulsar.orig \"\$stage/prev\" || exit 1
       $PROD_BT_RESTORE
       replace_pulsar pulsar.new
       sync && test_lifecycle_child ./ctl.sh restart" ;;
  restore)
    own
    # Если после краш-цикла supervise.sh удалил enabled (give_back), станцию возвращает ctl.sh enable.
    r "$TEST_HANDOFF
       cd $D && [ -f pulsar.orig ] || { echo 'no pulsar.orig' >&2; exit 1; }
       $PROD_REPLACE
       stage_replace
       cp -p pulsar.orig \"\$stage/pulsar\" || exit 1
       if [ -f pulsar.prev.orig ]; then ln pulsar.prev.orig \"\$stage/prev\" || exit 1; fi
       if [ pulsar.orig -ef pulsar.orig.bt-blocked ]; then ln \"\$stage/pulsar\" \"\$stage/guard\" || exit 1; fi
       $PROD_BT_RESTORE
       replace_pulsar \"\$stage/pulsar\"
       rm -f pulsar.orig pulsar.orig.bt-blocked pulsar.prev.orig ||
         echo 'restored the binary but could not remove swap backups' >&2
       sync
       if [ -f enabled ]; then test_lifecycle_child ./ctl.sh restart; else test_lifecycle_child ./ctl.sh enable; fi" ;;
  cleanup)
    own
    r "$TEST_LOCK
       test_lifecycle_lock '$D'
       $TEST_BT_RESTORED
       if [ -f $T/pid ] && kill -0 \$(cat $T/pid) 2>/dev/null; then echo 'test instance running: test-stop first' >&2; exit 1; fi
       rm -rf $T || exit 1
       $DMIX" ;;
  status) r "$D/ctl.sh status; ls -l $D; df -Pk $D; cat $L/owner 2>/dev/null || echo 'no lock'" ;;
  log) r "tail -n ${1:-50} /tmp/pulsar.log" ;;
  *) usage ;;
esac
