# deCONZ golden harness

Regenerates `satellite/crates/pulsar-zigbee/tests/golden/deconz.txt`: runs
`tests/golden/deconz_script.txt` against the C++ `DeconzServer` + `ZigbeeCoordinator` of tag
`rust-port-base` over a stub `Loop` and a stub stop-and-wait `TelinkLink` (`stub/`), with the
client on a loopback TCP socket through the real accept / read paths. The Rust test
`deconz::tests::deconz_matches_cxx_golden` replays the same script and compares.

```sh
git worktree add --detach ../pulsar-ref rust-port-base      # once
REF=../pulsar-ref/satellite/src
zig c++ -std=c++20 -O1 -Idocs/rust-port/tools/deconz_golden/stub -I$REF \
  docs/rust-port/tools/deconz_golden/deconz_harness.cpp \
  $REF/zigbee/deconz_server.cpp $REF/zigbee/coordinator.cpp -o /tmp/deconz_harness
/tmp/deconz_harness satellite/crates/pulsar-zigbee/tests/golden/deconz_script.txt \
  > satellite/crates/pulsar-zigbee/tests/golden/deconz.txt
```

zig 0.14.1 (`zig c++`) as for the station builds; any C++20 compiler on Linux works.
