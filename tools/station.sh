#!/bin/sh
# Station helper (docs/stations.md). Never commit station addresses.
# PULSAR_TAG: free-form tag of the work holding the lock (one word, no spaces).
set -eu
usage() {
  echo "usage: PULSAR_TAG=<tag> $0 midi|lite2 lock MINUTES|unlock|break-lock|stop|start|push FILE...|test-start|test-stop|test-log [N]|swap BINARY|restore|cleanup|status|log [N]" >&2
  exit 2
}
[ $# -ge 2 ] || usage
case "$1" in
  midi) H=${PULSAR_MIDI:?PULSAR_MIDI is not set} ;;
  lite2) H=${PULSAR_LITE2:?PULSAR_LITE2 is not set} ;;
  *) usage ;;
esac
cmd=$2
shift 2
L=/tmp/pulsar-agent.lock
T=/tmp/pulsar-test
D=/data/pulsar
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
# Remote snippet: remove a stale ALSA dmix lock (shm 0x400/0x401 nobody is attached to, then
# semaphore 0x400) left by a process killed while playing; without it the station stays silent
# until a reboot (satellite/deploy/supervise.sh header). Run where no pulsar is playing.
DMIX=$(cat <<'EOF'
r= b=0 s=$(ipcs -m 2>/dev/null)
for k in 400 401; do
  n=$(echo "$s" | awk -v k=0x00000$k '$1 == k { print $6; exit }')
  if [ "$n" = 0 ]; then ipcrm -M 0x$k && r="$r shm 0x$k"; elif [ -n "$n" ]; then b=1; fi
done
if [ $b = 0 ] && ipcs -s 2>/dev/null | awk '$1 == "0x00000400" { f = 1 } END { exit !f }'; then
  ipcrm -S 0x400 && r="$r sem 0x400"
fi
if [ -n "$r" ]; then echo "removed stale ALSA dmix lock (${r# })"; fi
EOF
)
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
  start) own; r "$D/ctl.sh reload" ;;
  push)
    [ $# -ge 1 ] || usage
    own; r "mkdir -p $T"
    for f; do b=$(basename "$f"); up "$f" "$T/$b"; done ;;
  test-start)
    own
    r "! pidof supervise.sh >/dev/null || { echo 'run stop first' >&2; exit 1; }
       [ -x $T/pulsar ] || { echo 'push pulsar first' >&2; exit 1; }
       grep -v -e '^volume=' -e '^volume2=' -e '^volume_curve=' -e '^muted=' -e '^auto_update=' -e '^update_attempt=' \
         $D/state.conf > $T/state.conf 2>/dev/null || true
       printf 'volume2=0.2\nmuted=0\nauto_update=0\n' >> $T/state.conf
       for z in $D/zigbee-*.ybl; do [ -f \"\$z\" ] && cp \"\$z\" $T/; done
       cd $T
       nohup env PULSAR_PORT_OFFSET=10000 ./pulsar $T/state.conf < /dev/null >> $T/log 2>&1 &
       echo \$! > $T/pid; sleep 2; kill -0 \$(cat $T/pid) && echo started" ;;
  test-stop)
    own
    r "p=\$(cat $T/pid) || exit 1; kill \$p 2>/dev/null
       i=0; while [ \$i -lt 10 ] && kill -0 \$p 2>/dev/null; do sleep 1; i=\$((i+1)); done
       kill -0 \$p 2>/dev/null && kill -9 \$p; rm -f $T/pid
       $DMIX
       echo stopped" ;;
  test-log) r "tail -n ${1:-50} $T/log" ;;
  swap)
    [ $# -eq 1 ] || usage
    own
    # The installed binary is kept as pulsar.orig and serves as .prev meanwhile, so a rollback by
    # supervise.sh lands on it; the owner's .prev waits in pulsar.prev.orig.
    r "[ -f $D/enabled ] || { echo 'Yandex mode (no $D/enabled): refusing' >&2; exit 1; }
       [ \$(df -Pk $D | awk 'NR==2 {print \$4}') -gt 30000 ] || { echo 'less than 30 MB free in $D' >&2; exit 1; }
       cd $D && { [ -f pulsar.orig ] || cp -p pulsar pulsar.orig; } \
         && { [ ! -f pulsar.prev ] || [ -f pulsar.prev.orig ] || cp -p pulsar.prev pulsar.prev.orig; }"
    up "$1" "$D/pulsar.new"
    r "cd $D && ./pulsar.new --version && cp -p pulsar.orig pulsar.prev \
       && mv pulsar.new pulsar && sync && ./ctl.sh restart" ;;
  restore)
    own
    r "cd $D && [ -f pulsar.orig ] || { echo 'no pulsar.orig' >&2; exit 1; }
       cd $D && cp -p pulsar.orig pulsar.tmp && mv pulsar.tmp pulsar && rm -f pulsar.orig || exit 1
       if [ -f pulsar.prev.orig ]; then mv pulsar.prev.orig pulsar.prev; fi
       sync
       if [ -f enabled ]; then ./ctl.sh restart; else ./ctl.sh enable; fi" ;;
  cleanup)
    own
    r "if [ -f $T/pid ] && kill -0 \$(cat $T/pid) 2>/dev/null; then echo 'test instance running: test-stop first' >&2; exit 1; fi
       rm -rf $T || exit 1
       $DMIX" ;;
  status) r "$D/ctl.sh status; ls -l $D; df -Pk $D; cat $L/owner 2>/dev/null || echo 'no lock'" ;;
  log) r "tail -n ${1:-50} /tmp/pulsar.log" ;;
  *) usage ;;
esac
