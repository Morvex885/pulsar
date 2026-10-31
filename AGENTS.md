# AGENTS.md

Pulsar is a native Rust daemon for rooted Yandex Stations, exposing ESPHome to Home Assistant,
Sendspin to Music Assistant, and the Midi Zigbee radio to ZHA / Zigbee2MQTT. Proprietary Yandex
libraries are loaded from the station with `dlopen`; never copy them into the repository.

## Read for the task

- User behaviour, installation, build prerequisites: [README.md](README.md).
- Runtime, actors, FFI, state persistence and service contracts: [docs/architecture.md](docs/architecture.md).
  Read before changing daemon code.
- Deployment, rollback, updater/release contracts and diagnostics: [docs/development.md](docs/development.md).
  Read before changing the installer, updater, supervisor or release workflow.
- Hardware, capture layouts and Yandex C API: [docs/hardware.md](docs/hardware.md);
  Mini 3 specifics: [docs/mini3.md](docs/mini3.md). Read before changing model support or firmware bindings.
- Zigbee framing, flash layout, bootloader and coordinator: [docs/zigbee.md](docs/zigbee.md).
  Read before changing radio code or firmware.
- Live device work: [docs/stations.md](docs/stations.md) and `tools/station.sh` (required).

## Code map

- `satellite/`: Rust workspace; `crates/pulsar/` owns the controller and settings.
  `pulsar-core` holds shared types and `Model`; other `pulsar-*` crates own individual services.
  Full workspace map: [architecture](docs/architecture.md#workspace).
- `satellite/crates/*/src/bin/`: diagnostic probes; `tools/`: station helper and Python clients.
- `satellite/deploy/`: device scripts and release packaging; `satellite/third_party/`: pinned C++ dependencies.
- `installer/`: Go installer; `radio/`: Telink firmware; `ssh/`: dropbear build and startup.

## Build and validation

```sh
make -C satellite ARCH=armhf           # or ARCH=aarch64; omit ARCH to build both
make -C satellite ARCH=armhf tools     # diagnostic binaries
make -C satellite check               # fmt, clippy (host + both targets), host tests
go -C installer test ./...
radio/build.sh                        # when changing radio firmware
ssh/build.sh                          # when changing bundled SSH
```

Use the pinned Rust toolchain, cargo-zigbuild 0.24.0 and zig 0.14.1; Go 1.27+ for the installer.
Zig ≤ 0.13 produces broken armhf C++ release builds. See README for remaining prerequisites.
Run checks relevant to the change; daemon changes must pass `make -C satellite check`.
Host tests do not verify device behaviour or the linked Sendspin implementation.

## Development rules

- One current-thread tokio runtime with a `LocalSet`; use `pulsar_core::actors::spawn_actor`.
  Handles never block; blocking/real-time work runs on named threads. Follow the architecture doc.
- Keep `unsafe` / libc in the existing FFI/sys modules, compat shim and crash handler;
  document safety. Do not add or remove `mul_add` fusions: golden tests pin rounding.
- Pin external Rust dependencies in `[workspace.dependencies]`; justify new dependencies.
- Preserve `state.conf` compatibility with `.prev`; new keys need safe defaults. Use
  `persist_now()` when a change must survive an immediate restart, crash or binary swap.
- Investigate hardware before adding models. Choose ABI by available userland loaders, not
  kernel architecture; add a second ABI only for a userland actually observed on that model.
- Comments in English explain device facts and constraints. Keep technical detail in the
  relevant doc or module rather than expanding this file.

## Git and repository hygiene

- Work on short-lived `feat/…`, `fix/…` or `docs/…` branches and submit a PR. No direct pushes,
  force pushes or deletion of `main`; squash or rebase only. Required checks:
  `build (aarch64)` and `build (armhf)`. Never move or delete release tags `v*`.
- Never publish device IPs, MACs, serials, Wi-Fi names, tokens or device logs.
  Use `192.168.1.50` in examples. Keep ignored `backup/`, `dump/`, `re/`, `ref/` out of commits.

## Device safety

- Never break root adb access. Do not touch adbd, `/data/quasar/activate_*` or
  `/data/quasar/rootkeeper.sh`; the installer may only remove its legacy `hookSat` / `hookSSH` lines.
- Persistent changes belong in `/data`, never `/system` or `/etc`. The sole exception is
  `/etc/init.d/S99zpulsar`, written by the installer after its rootfs protection checks, with `/`
  remounted rw only for the write and restored ro. The guard requires no `verity`/`dm=` in
  `/proc/cmdline`, only `control` in `/dev/mapper`, and `/etc` plus `/system/vendor/quasar` on
  the same filesystem mounted at `/`. Firmware OTA protection is outside pulsar's scope.
- Always use `adb -s <serial>`. `adb shell` is a pty, not binary-safe, and returns exit status 0;
  use push/pull for files and the installer's `sh()` exit marker for commands.
- Preserve the working binary as `.prev` before replacing it. Follow the station protocol for
  lock, stopping services, test instances, restoration and cleanup.
- Hardware and ESPHome API tests require the service stopped. Never connect test ESPHome or
  deCONZ clients to the live HA / Z2M / ZHA ports. Use the isolated test instance and alternate ports.
- No live volume changes or test sounds without owner consent. Audio tests must cap volume at
  ≤ 0.2 on the Mixer scale (raw ≤ 82 where hardware volume exists).
- Never drive Zigbee reset/boot by raw GPIO number: use the `zb_control` paths. Flash only the
  current role's image; never flash with role `off`. Follow the station protocol's backup
  requirement before a live swap on a coordinator.
- `/tmp` is limited tmpfs; remove test files and clean stale dmix locks through `tools/station.sh`.
