# Build, deployment and release reference

Paths below are relative to the repository root unless absolute.
Build prerequisites and commands: [README](../README.md#сборка-из-исходников).
Runtime and linking rules: [architecture](architecture.md).
Live tests: [station protocol](stations.md).

## Diagnostics

`make -C satellite ARCH=<arch> tools` builds `crates/*/src/bin/*.rs` into
`satellite/build/<arch>/tools/`. Build one with
`make -C satellite ARCH=armhf build/armhf/tools/alsa_abi_probe`.
`api_selftest` arguments: `[port=16053] [name] [run_seconds=120] [key_file]`.
The other probes describe their options in their source; station safety rules apply to all of them.

Deploy the stripped `pulsar`; keep `pulsar.debug` for `addr2line -f -e` against `=== CRASH`
reports. Builds are non-PIE (`-C relocation-model=static`), so addresses are absolute;
`handler=` identifies `pulsar_crash_handler`. aarch64 adds an `fp:` frame-pointer chain;
armhf reports pc/lr only. Host Rust tests do not link sendspin-cpp; device behaviour needs
station tests.

## CI and releases

CI (`.github/workflows/build.yml`) builds the daemon and every tool for both architectures, runs
`make -C satellite check` (job `check`) and builds the radio firmware images (job `radio`) on each
pull request and push to `main`; the binaries are attached to the run as artifacts. Job
`installer` vets the installer for linux/windows/darwin, runs its tests and cross-builds it for
windows/amd64, darwin/{arm64,amd64} and linux/{amd64,arm64}. Pushing a tag `v*` additionally builds
dropbear (job `ssh`, `ssh/build.sh`) and publishes a GitHub release with one archive per host OS,
`pulsar-{windows-amd64.zip,macos-{arm64,amd64}.tar.gz,linux-{amd64,arm64}.tar.gz}` (assembled by
`satellite/deploy/dist.sh`: `pulsar-install`, both daemon binaries, `radio/out/zigbee-*.ybl`,
dropbear and the device scripts in the repo layout), plus `pulsar-<arch>.bin`, `pulsar-<arch>.debug`,
`zigbee-coordinator.ybl`, `zigbee-router.ybl` and `SHA256SUMS`. Asset names are constant (see
[Updates from HA](#updates-from-ha)). Only stable releases are offered (`releases/latest`); the tag must equal the
binary's `git describe` (the build job checks out the tag, so it does).

## Deploy

```sh
go -C installer run . --list         # discover stations (mDNS; --scan = adb port scan of the /24) + model probe
go -C installer run . <ip>[:port]    # build if needed, copy binary (+ .prev), install autostart when permitted, restart
```

The installer writes `/etc/init.d/S99zpulsar` only when the rootfs guard permits it.
With dm-verity, device-mapper or `/etc` on a separate filesystem, it does not write the file.
An existing init-script hook or an active legacy `rootkeeper.sh` hook called by `onboot.sh`
may still provide autostart; without either, pulsar will not start automatically after reboot.
The installer reports this condition and leaves the existing hooks in place.

Models are identified by `/proc/device-tree/compatible` (`yandex,<codename>`) both by the daemon
(`detect_model()`, exits on unknown hardware) and by the installer (`modelName()`, `abis()`,
`archOf()` in `installer/probe.go`). The Mini has no `yandex,*` entry, and its board id alone means
nothing: all three sites — the daemon (`mini_from_board_and_cfg()`), the installer and `boot.sh` —
require BOTH `/proc/device-tree/amlogic-dt-id = axg_s420_v03` (an Amlogic S420 reference-board id,
not Yandex-specific) AND `/system/vendor/quasar/quasar.cfg` naming `deviceType=yandexmini`. The
installer skips a station whose adb is not root (`id -u`) before probing its loaders.
`/data/pulsar/asound.conf` (Mini first generation) is written by the installer, not by HA
updates: an update replaces only the binary and the radio images, so a change to
`satellite/deploy/asound.conf` (e.g. a dmix fix) reaches an installed station only when
`pulsar-install` runs again.

The binary is `satellite/build/<arch>/pulsar` for the model's first ABI in
`abis()` whose dynamic loader is on the station (`/lib/ld-linux-aarch64.so.1`,
`/lib/ld-linux-armhf.so.3`): its usual one, or for the Midi the armhf build on images with only
that loader (#13). A Lite 2, Mini 2, Mini 3 or Mini without the armhf loader is skipped (no
64-bit userspace has been seen on them, and the aarch64 binary needs glibc 2.34). Adding a model:
hardware study first (ALSA capture layout, VQE preset, spotter channels, LEDs, clock, inputs,
earcons), then a `Model` in `satellite/crates/pulsar-core/src/model.rs` and `modelName()` /
`abis()` entries. A second ABI in `abis()` is only for a userspace that has actually been seen on
that model.

Manual binary update (e.g. over ssh):

```sh
scp satellite/build/<arch>/pulsar root@<ip>:/data/pulsar/pulsar.new
ssh root@<ip> 'cd /data/pulsar && cp pulsar pulsar.prev &&
  mv pulsar.new pulsar && ./ctl.sh restart'
```

Device layout: `/data/pulsar/{pulsar,.prev,.bad,boot.sh,supervise.sh,
ctl.sh,state.conf,api_key,enabled,sendspin/,zigbee-coordinator.ybl,zigbee-router.ybl,asound.conf}`
(`asound.conf`: Mini only). Outside
/data: `/etc/init.d/S99zpulsar` (autostart, written by the installer). `api_key`: ESPHome Noise PSK
(base64 of 32 bytes, 0600), written by HA's auto-provisioning or `ctl.sh api-key`; absent =
plaintext (Noise with the all-zeros key is then accepted so HA can provision); corrupt = API fails
closed. `.bad`: binary replaced by the crash-loop rollback. Log: `/tmp/pulsar.log`
(`ctl.sh log N`; rotated at 1 MB by `supervise.sh`, one `.1` kept). Boot chain: busybox init →
`rcS` (runs `/etc/init.d/S??*` in order with `start`, waiting for each) → `S97quasar` (Midi) /
`S99quasar` (Lite 2, Mini 2) / `S99onboot.sh` (Mini) → `onboot.sh` (adbd via `activate_adb.sh check`, quasar in
background; returns); then `S99zpulsar` (sorts after `S99quasar`; `S99pulsar` would not) →
`boot.sh` in background (once per boot: `/tmp/pulsar-boot.lock`) → waits for quasar, `q stop` →
`supervise.sh` restart loop while `enabled` exists. `ctl.sh reload` restarts the supervisor itself
(needed after replacing `supervise.sh`; the installer does it when the script changed).

Supervisor (contract in the `supervise.sh` header): exit 0 is a deliberate restart (`ctl.sh`,
HA "Restart") only after at least `MINUP=5` seconds of uptime. A shorter exit, even with
status 0, counts as a short crash. Five short crashes trigger rollback to `.prev` (bad binary
kept as `.bad`); if there is no different previous binary, or it also crash-loops, the supervisor
removes `enabled` and starts the Yandex stack. Only a run of at least `LONG` seconds
(300 by default) resets the crash count; deliberate shorter restarts leave it unchanged.
Before every launch it removes a stale ALSA dmix lock and logs `=== alsa: removed stale dmix lock (…)`; the installer's health check
(a marker ~10 s after the restart, then the log) must not treat that line as a failure.

### Updates from HA

`pulsar-update`, entities "Firmware" + "Auto update"; user-facing behaviour in
README, "Обновление из Home Assistant". What must not break:

- Asset names are constant: README links `releases/latest/download/...`, and the updater downloads
  `releases/download/<tag>/pulsar-<arch>.bin`, `zigbee-coordinator.ybl` and `zigbee-router.ybl`
  (Midi; each skipped when the release has none) and `SHA256SUMS`.
- The latest release is found through the redirect of `https://github.com/<repo>/releases/latest`
  to `/releases/tag/<tag>`, not the REST API: its 60 requests/hour per public IP are exhausted
  behind shared VPN / CGNAT exits.
- `state.conf` `update_attempt=<tag>` is set before the swap and cleared by the new binary; still
  set under another version means `supervise.sh` rolled it back, so auto-update skips that tag.

## Installer implementation

- `installer/` (Go) is the only host-side installer. Device commands are busybox-sh strings sent
  through `sh()` (adb shell plus `__rc=` marker; one command ≤ 4 KB on these firmwares).
  The host side uses only the Go stdlib and `golang.org/x/net`, no host tools except adb (plus
  make / `ssh/build.sh` in a checkout). The pure parts (init script `initScript`, rootfs check
  `rootfsGuard` / `rootMountRW`, rootkeeper clean-up `unhookRootkeeper` / `hookActive` /
  `checkRootkeeper`) are covered by `autostart_test.go` and `rootkeeper_test.go`; keep them pure.
