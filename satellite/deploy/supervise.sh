#!/bin/sh
# Keeps cucumber-satellite running while /data/cucumber-satellite/enabled exists.
# Log: /tmp/cucumber-satellite.log (tmpfs, rotated at 1 MB).
D=/data/cucumber-satellite
LOG=/tmp/cucumber-satellite.log
while [ -f "$D/enabled" ]; do
  if [ -f "$LOG" ] && [ "$(wc -c < "$LOG")" -gt 1000000 ]; then mv "$LOG" "$LOG.1"; fi
  echo "=== start $(date)" >> "$LOG"
  "$D/cucumber-satellite" "$D/state.conf" >> "$LOG" 2>&1
  echo "=== exit $? $(date)" >> "$LOG"
  sleep 3
done
