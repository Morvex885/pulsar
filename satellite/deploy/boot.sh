#!/bin/sh
# Boot hook, started in background by /etc/init.d/S99zpulsar (written by pulsar-install), which rcS
# runs after the firmware's S97quasar / S99quasar (onboot.sh: adbd, quasar in background). Stations
# set up by older installers may also start it from /data/quasar/rootkeeper.sh.
# If /data/pulsar/enabled exists: stop the Yandex stack started by onboot.sh and run
# pulsar under a restart loop. adbd and dropbear are never touched.
D=/data/pulsar
[ -f "$D/enabled" ] || exit 0
# Never stop the Yandex stack when there is nothing to replace it with.
[ -x "$D/pulsar" ] && [ -x "$D/supervise.sh" ] || exit 0
# Once per boot: two hooks (S99zpulsar and a leftover rootkeeper.sh line) may start this at the same
# time, and both would pass the pidof checks below before either supervisor exists. Never removed:
# releasing it before the exec would reopen that window; /tmp is tmpfs, so a reboot clears it.
mkdir /tmp/pulsar-boot.lock 2>/dev/null || exit 0
pidof pulsar >/dev/null 2>&1 && exit 0
pidof supervise.sh >/dev/null 2>&1 && exit 0  # never two supervisors
# Supported models only (see src/model.cpp); anything else keeps the Yandex stack.
tr '\0' '\n' </proc/device-tree/compatible | grep -qxE 'yandex,(cucumber|plum)' || exit 0

# onboot.sh has started quasar in background before S99zpulsar runs; wait for it, then stop it.
i=0
while [ $i -lt 30 ] && ! pidof quasar_launcher2 >/dev/null 2>&1; do
  sleep 1
  i=$((i + 1))
done
/system/vendor/quasar/q stop >/dev/null 2>&1

exec setsid "$D/supervise.sh" </dev/null >/dev/null 2>&1
