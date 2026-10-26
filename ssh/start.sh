#!/bin/sh
# Starts dropbear SSH on port 22 (root, blank password allowed: -B; keys: see authorized_keys below).
# Installed to /data/dropbear; started at boot by /etc/init.d/S99zpulsar (written by pulsar-install).
# Independent of adbd: never touches adb.
D=/data/dropbear
cd "$D" || exit 1
for t in dropbear dropbearkey scp dbclient; do
  [ -e "$D/$t" ] || ln -sf dropbearmulti "$D/$t"
done
for t in ed25519 ecdsa rsa; do
  [ -s "$D/host_$t" ] || "$D/dropbearkey" -t $t -f "$D/host_$t" >/dev/null 2>&1
done
pidof dropbear >/dev/null 2>&1 && exit 0
# Public keys: with $D/authorized_keys (0600, root) dropbear reads its keys from there instead of
# /root/.ssh, which is on the read-only rootfs of the Lite 2, Mini 2 and Mini 3.
AK=
if [ -s "$D/authorized_keys" ]; then
  # dropbear checks this directory, the file and every parent: root-owned, not group/world-writable.
  chmod go-w "$D"
  chmod 600 "$D/authorized_keys"
  AK="-D $D"
fi
# shellcheck disable=SC2086  # AK is empty or two words
exec "$D/dropbear" -B -p 22 -P /run/dropbear.pid $AK \
  -r "$D/host_ed25519" -r "$D/host_ecdsa" -r "$D/host_rsa"
