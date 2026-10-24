#!/bin/sh
# Station helper for the Rust port (docs/rust-port/STATIONS.md). Never commit station addresses.
set -eu
usage() {
  echo "usage: PULSAR_WP=WP-XX $0 midi|lite2 lock MINUTES|unlock|break-lock|stop|start|push FILE...|test-start|test-stop|test-log [N]|swap-rust BINARY|restore-cxx|cleanup|status|log [N]" >&2
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
r() { ssh -o BatchMode=yes -o ConnectTimeout=10 "$H" "$@"; }
# State-changing commands run only under this agent's own lock.
own() {
  wp=${PULSAR_WP:?PULSAR_WP is not set}
  r "read o _ 2>/dev/null < $L/owner && [ \"\$o\" = '$wp' ]" || { echo "no lock held by $wp" >&2; exit 1; }
}
case "$cmd" in
  lock)
    [ $# -eq 1 ] || usage
    wp=${PULSAR_WP:?PULSAR_WP is not set}
    case "$1" in '' | *[!0-9]*) usage ;; esac
    { [ "$1" -ge 1 ] && [ "$1" -le 180 ]; } || usage
    # The owner file appears complete or not at all (a lock dir without one is a crashed lock).
    r "mkdir $L 2>/dev/null && echo \"$wp \$(date +%s) $1\" > $L/owner.tmp && mv $L/owner.tmp $L/owner" \
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
       sleep 1; ! pidof pulsar supervise.sh >/dev/null && echo stopped" ;;
  start) own; r "$D/ctl.sh reload" ;;
  push)
    [ $# -ge 1 ] || usage
    own; r "mkdir -p $T"
    for f; do b=$(basename "$f"); r "cat > $T/$b && chmod 755 $T/$b" < "$f"; done ;;
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
       kill -0 \$p 2>/dev/null && kill -9 \$p; rm -f $T/pid; echo stopped" ;;
  test-log) r "tail -n ${1:-50} $T/log" ;;
  swap-rust)
    [ $# -eq 1 ] || usage
    own
    r "[ -f $D/enabled ] || { echo 'Yandex mode (no $D/enabled): refusing' >&2; exit 1; }
       [ \$(df -Pk $D | awk 'NR==2 {print \$4}') -gt 30000 ] || { echo 'less than 30 MB free in $D' >&2; exit 1; }
       cd $D && { [ -f pulsar.cxx ] || cp -p pulsar pulsar.cxx; } \
         && { [ ! -f pulsar.prev ] || [ -f pulsar.prev.orig ] || cp -p pulsar.prev pulsar.prev.orig; }"
    r "cat > $D/pulsar.new" < "$1"
    r "cd $D && chmod 755 pulsar.new && ./pulsar.new --version && cp -p pulsar.cxx pulsar.prev \
       && mv pulsar.new pulsar && sync && ./ctl.sh restart" ;;
  restore-cxx)
    own
    r "cd $D && [ -f pulsar.cxx ] || { echo 'no pulsar.cxx' >&2; exit 1; }
       cd $D && cp -p pulsar.cxx pulsar.tmp && mv pulsar.tmp pulsar && rm -f pulsar.cxx || exit 1
       if [ -f pulsar.prev.orig ]; then mv pulsar.prev.orig pulsar.prev; fi
       sync
       if [ -f enabled ]; then ./ctl.sh restart; else ./ctl.sh enable; fi" ;;
  cleanup)
    own
    r "if [ -f $T/pid ] && kill -0 \$(cat $T/pid) 2>/dev/null; then echo 'test instance running: test-stop first' >&2; exit 1; fi
       rm -rf $T" ;;
  status) r "$D/ctl.sh status; ls -l $D; df -Pk $D; cat $L/owner 2>/dev/null || echo 'no lock'" ;;
  log) r "tail -n ${1:-50} /tmp/pulsar.log" ;;
  *) usage ;;
esac
