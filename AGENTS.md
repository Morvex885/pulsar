# AGENTS.md

Guide for coding agents working on pulsar. User-facing docs: `README.md` (Russian). Execution
model, actor and FFI rules, per-service behaviour: `docs/architecture.md` (Russian). Testing on
live stations: `docs/stations.md` (Russian) with `tools/station.sh`.

## What this is

A native Rust daemon replacing the Yandex stack (`maind` / `quasar_launcher2`) on rooted Yandex
Stations: Midi (`cucumber`: usually aarch64, glibc 2.36, 1 GB RAM; some images run a 32-bit
armhf userland) and Lite 2 (`plum`: 32-bit armhf, glibc 2.30, 237 MB RAM). It exposes the speaker
to Home Assistant over the ESPHome Native API (port 6053; plaintext until a Noise key is
provisioned, then Noise) and to Music Assistant as a Sendspin player (port 8928); on the Midi
its Zigbee radio runs pulsar's own firmware (`radio/`): a deCONZ-protocol network coordinator for
Zigbee2MQTT / ZHA (port 6638) or a router in another network (e.g. Zigbee2MQTT). Wake word, command
spotting and echo cancellation use the firmware's own proprietary libraries, loaded with `dlopen`
at runtime; they are never copied into the repo. Stations on one LAN arbitrate the wake word over
UDP broadcast (port 6054) so that only the one that heard it loudest answers.

## Layout

| Path | Content |
|---|---|
| `satellite/Cargo.toml` | Cargo workspace (edition 2024, every external dependency pinned in `[workspace.dependencies]`), `rust-toolchain.toml`, `Makefile` (cargo-zigbuild cross builds, `make check`) |
| `satellite/crates/pulsar/` | the daemon: `main.rs` (runtime, teardown watchdog, final settings flush), `crash.rs` (crash report), `controller/` (all satellite logic: voice pipeline state machine, entities, local commands, updates; `logic.rs` holds its pure decisions), `settings.rs` (`state.conf`: format readable by the binary kept as `.prev`, durable writes, `StateFile` debounce; see Conventions); `PULSAR_PORT_OFFSET=<n>` adds n to the ESPHome, Sendspin, deCONZ and arbiter ports and turns mDNS off (test instance next to the running daemon) |
| `satellite/crates/pulsar-core/` | logging (stderr + ESPHome log sink), `Timers` (per-actor timers, firing order in `timers.rs`), actor registry (`spawn_actor`, `join_all`), sysfs / network / TZ helpers, `Model` (per-model hardware description: capture layout, VQE preset, LEDs, clock, inputs, sounds, Zigbee lines; detected from `/proc/device-tree/compatible`), `AmbientDimmer`, `MusicFrame`, `UiState`, `ClockShot`, `fmt` (`state.conf` number format) |
| `satellite/crates/pulsar-alsa/` | libasound loaded at runtime (dlopen), PCM / ctl wrappers, the versioned hw-params ABI of armhf alsa-lib (`__snd_pcm_hw_params_*` first); bin `alsa_abi_probe` |
| `satellite/crates/pulsar-esphome/` | ESPHome API server: transport (plaintext / Noise), clients and the voice owner (`server.rs`), entities and states (`server/entities.rs`), commands and voice assistant messages (`server/commands.rs`), hand-written protobuf (`proto.rs`), Noise key (`key.rs`); bin `api_selftest`; client `tools/esphome_check.py` |
| `satellite/crates/pulsar-lan/` | mDNS responder (`mdns.rs`) and the wake word arbiter between stations (`arbiter.rs`: claims over UDP broadcast :6054, 250 ms window, best claim answers); bin `lan_probe` |
| `satellite/crates/pulsar-audio/` | microphone front-end (`frontend.rs`): capture thread (ALSA → FIFO) + processing thread: Yandex VQE → wake + command spotters → 16 kHz stream; music features of the speaker reference (`spectrum.rs`: 512-point FFT at 16 kHz → 8 bands, loudness, beats as `MusicFrame`) while the visualization can be shown; C API of libyandex_evil (`engine/`); bins `ww_offline`, `frontend_probe` |
| `satellite/crates/pulsar-media/` | playback through the firmware's GStreamer (`libquasar_gstreamer.so`, dlopen, `gst.rs`), channels Music / Announce on a GLib thread; `Mixer` = ALSA "Master Playback Volume" (softvol of the `speakers` PCM); bin `player_probe` |
| `satellite/crates/pulsar-sendspin/` | Sendspin player on top of sendspin-cpp (C shim in `shim/`, linked statically by `build.rs` in the Makefile builds), own ALSA sink on the `speakers` dmix; roles player, metadata, controller, visualizer (8 log bands 40 Hz–7.6 kHz, loudness, beats → `MusicFrame`); bin `sendspin_probe` |
| `satellite/crates/pulsar-hw/` | LED ring / logo (framebuffers behind ledfb triggers; `leds.rs`: output, case matrix, layer compositor, fades), pure rendering of voice states / one-shots / effects / music styles (`led_fx.rs`), input (touch keys, `SW_MUTE_DEVICE`; `input.rs`); bin `hw_probe` |
| `satellite/crates/pulsar-clock/` | clock (sysfs segments, per-LED brightness; `clock.rs`: layers), pure rendering of the clock's stock animations and music levels (`clock_fx.rs`), font, light sensor (`ltr308als01`, `light_sensor.rs`) |
| `satellite/crates/pulsar-zigbee/` | Midi Zigbee radio: zbhci link to the Telink chip (`link.rs`), coordinator on pulsar's firmware + ZDO to the coordinator itself (`coordinator.rs`), deCONZ-protocol TCP server for Zigbee2MQTT / ZHA (`deconz.rs`), router control (`router.rs`), `.ybl` parsing + XMODEM upload through the radio's bootloader (`flasher.rs`), role / firmware orchestration (`service.rs`), role table (`roles.rs`), byte order helpers (`bytes.rs`); bin `zigbee_probe [port] \| info \| flash <image>` (coordinator + deCONZ server on another port without the daemon; firmware identification; writing an image) |
| `satellite/crates/pulsar-update/` | HA "Firmware" update entity backend: HTTPS with rustls + ring (`https.rs`), GitHub release check / download / verify / swap (`updater.rs`), version comparison (`version.rs`); bin `update_probe` |
| `satellite/crates/*/src/bin/` | tools (`make ARCH=<arch> tools`): `alsa_abi_probe` (ALSA hw-params ABI on the `null` PCM, no sound), `api_selftest` (API server with printing handlers, `[port=16053] [name] [run_seconds=120] [key_file]`), `lan_probe` (mDNS / arbiter probes on alternative ports), `ww_offline` (offline spotter test on a raw capture WAV), `frontend_probe` (microphone front-end without the daemon: events, rates, `--record` of the raw capture), `player_probe` (music / announce / http / 404 / https scenario at volume ≤ 0.2, original raw restored), `sendspin_probe` (Sendspin player without the daemon on another port, Master volume capped at 0.2, for `tools/sendspin_server.py`), `hw_probe` (LEDs, clock, input, light sensor), `update_probe` (HTTPS client, release check, install into a scratch dir), `zigbee_probe` |
| `satellite/deploy/` | `dist.sh` (release archives, host side); `boot.sh`, `supervise.sh`, `ctl.sh` (device side) |
| `satellite/third_party/` | `fetch.sh` (pinned deps), CMake glue, zig toolchain wrappers (sendspin-cpp only; with the shim in `crates/pulsar-sendspin/shim/` the only C++ in the build) |
| `installer/` | `pulsar-install` (Go): station discovery (mDNS legacy-unicast query / `--scan` port scan), adb (downloads platform-tools 37.0.1 if missing), install (incl. `/etc/init.d/S99zpulsar`), health check |
| `ssh/` | `build.sh` (static dropbear + sftp-server), `start.sh` (device side) |
| `radio/` | Zigbee firmware for the Midi radio on the Telink Zigbee SDK (C): `build.sh` (pinned SDK + toolchain, `sdk.patch`), `Makefile`, `coordinator/` and `router/` (apps), `ybl.c` (host tool: binary → `.ybl`) |
| `docs/` | `architecture.md` (execution model, rules for all crates, floating point, sendspin-cpp link recipe, per-service behaviour), `stations.md` (station test protocol) |
| `tools/` | `station.sh` (station lock, stop / start, push, test instance, live swap, dmix cleanup), test clients `esphome_check.py`, `sendspin_server.py`, `deconz_read.py` (Python, `requirements.txt`) |

Git-ignored and never to be published: `backup/` (full device backups), `dump/` (firmware binaries,
logs), `re/` (Ghidra projects and decompiled Yandex code), `ref/` (upstream checkouts).

## Build

```sh
make -C satellite -j"$(nproc)"                       # both: build/{aarch64,armhf}/pulsar
make -C satellite ARCH=armhf                          # one architecture
make -C satellite ARCH=armhf tools                    # every tool → build/armhf/tools/
make -C satellite ARCH=armhf build/armhf/tools/alsa_abi_probe   # one tool (crates/*/src/bin/<name>.rs)
make -C satellite check                              # host: cargo fmt --check, clippy -D warnings (host + both targets via cargo-zigbuild, needs zig of any version), tests
ssh/build.sh                                         # → ssh/out/{aarch64,armhf}/{dropbearmulti,sftp-server}
radio/build.sh                                       # → radio/out/zigbee-{coordinator,router}.ybl (needs libc6:i386, unzip)
go -C installer test ./...                           # installer unit tests
go -C installer run . [args]                         # installer from a checkout (builds the daemon first)
```

Host tools: rustup (installs the toolchain pinned in `satellite/rust-toolchain.toml` with both
targets), **cargo-zigbuild 0.24.0** (`cargo install cargo-zigbuild --locked --version 0.24.0`;
also for `make check`), **zig 0.14.1** (`ZIG=/path/to/zig` if not in PATH; arch builds reject
≤ 0.13, whose armhf C++ release builds (sendspin-cpp) crash before `main`; `make check` takes any
zig: its station clippy only compiles ring's C), a host C compiler (`cc`, for `make check`), make,
git, curl, patch; cmake ≥ 3.16 and ninja for sendspin-cpp only; Go 1.27+ (installer). The first
build fetches pinned dependencies into `satellite/third_party/src/` and builds sendspin-cpp per
architecture into `satellite/build/<arch>/third_party/`; cargo's output is in `satellite/target/`.
libasound is loaded at runtime (`dlopen`, `pulsar-alsa`), nothing ALSA is linked. Outputs per arch:
`pulsar` (stripped, the one deployed) and `pulsar.debug` (unstripped, line tables) for
`addr2line -f -e` on the crash report the daemon writes to the log (`=== CRASH`; non-PIE,
`-C relocation-model=static`: addresses are absolute; `handler=` is the address of
`pulsar_crash_handler`; aarch64 adds an `fp:` frame-pointer chain, armhf reports pc/lr only). The
version is `git describe --tags --match 'v*' --always --dirty` (`--version`, HA DeviceInfo and the
"Version" entity; release tags only, so the updater can parse it). Host-side tests cover the Rust
crates (x86_64, without sendspin-cpp) and the installer; hardware behaviour is verified on the
device.

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
"Updates from HA"). Only stable releases are offered (`releases/latest`); the tag must equal the
binary's `git describe` (the build job checks out the tag, so it does).

## Branches

`main` is the only long-lived branch, protected by a repository ruleset: no direct pushes, force
pushes or deletion; changes land through a pull request with green `build (aarch64)` and
`build (armhf)` checks, squash or rebase only (linear history). Work on a short-lived branch
(`feat/…`, `fix/…`, `docs/…`), open a PR, merge; the branch is deleted on merge. Release tags `v*`
cannot be moved or deleted. Nothing personal goes into the repository: no real IPs, MACs, serials,
Wi-Fi names, tokens or device logs (`192.168.1.50` is the documentation placeholder). Commits made
for the maintainer (including by agents) use `Morvex885
<278529280+Morvex885@users.noreply.github.com>`, no real names or e-mails; contributors' commit
identities are their own business (a squash merge takes the PR author's GitHub identity).

## Deploy

```sh
go -C installer run . --list         # discover stations (mDNS; --scan = adb port scan of the /24) + model probe
go -C installer run . <ip>[:port]    # build if needed, copy binary (+ .prev), write /etc/init.d/S99zpulsar, restart
```

Models are identified by `/proc/device-tree/compatible` (`yandex,<codename>`) both by the daemon
(`detect_model()`, exits on unknown hardware) and by the installer (`modelName()`, `abis()`,
`archOf()` in `installer/probe.go`). The installer skips a station whose adb is not root (`id -u`)
before probing its loaders. The binary is `build/<arch>/pulsar` for the model's first ABI in
`abis()` whose dynamic loader is on the station (`/lib/ld-linux-aarch64.so.1`,
`/lib/ld-linux-armhf.so.3`): its usual one, or for the Midi the armhf build on images with only
that loader (#13). A Lite 2 without the armhf loader is skipped (no 64-bit plum firmware has
been seen, and the aarch64 binary needs glibc 2.34). Adding a model: hardware study first (ALSA
capture layout, VQE preset, spotter channels, LEDs, clock, inputs, earcons), then a `Model` in
`satellite/crates/pulsar-core/src/model.rs` and `modelName()` / `abis()` entries. A second ABI in
`abis()` is only for a userspace that has actually been seen on that model.

Manual binary update (e.g. over ssh):

```sh
scp satellite/build/<arch>/pulsar root@<ip>:/data/pulsar/pulsar.new
ssh root@<ip> 'cd /data/pulsar && cp pulsar pulsar.prev &&
  mv pulsar.new pulsar && ./ctl.sh restart'
```

Device layout: `/data/pulsar/{pulsar,.prev,.bad,boot.sh,supervise.sh,
ctl.sh,state.conf,api_key,enabled,sendspin/,zigbee-coordinator.ybl,zigbee-router.ybl}`. Outside
/data: `/etc/init.d/S99zpulsar` (autostart, written by the installer). `api_key`: ESPHome Noise PSK
(base64 of 32 bytes, 0600), written by HA's auto-provisioning or `ctl.sh api-key`; absent =
plaintext (Noise with the all-zeros key is then accepted so HA can provision); corrupt = API fails
closed. `.bad`: binary replaced by the crash-loop rollback. Log: `/tmp/pulsar.log`
(`ctl.sh log N`; rotated at 1 MB by `supervise.sh`, one `.1` kept). Boot chain: busybox init →
`rcS` (runs `/etc/init.d/S??*` in order with `start`, waiting for each) → `S97quasar` (Midi) /
`S99quasar` (Lite 2) → `onboot.sh` (adbd via `activate_adb.sh check`, quasar in background;
returns); then `S99zpulsar` (sorts after `S99quasar`; `S99pulsar` would not) → `boot.sh` in
background (once per boot: `/tmp/pulsar-boot.lock`) → waits for quasar, `q stop` →
`supervise.sh` restart loop while `enabled` exists. `ctl.sh reload` restarts the supervisor itself
(needed after replacing `supervise.sh`; the installer does it when the script changed).

Supervisor (contract in the `supervise.sh` header): exit 0 = deliberate restart (`ctl.sh`, the HA
"Restart" button), not a crash; a crash loop swaps in `.prev` (bad one kept as `.bad`), and if that
crash-loops too, removes `enabled` and starts the Yandex stack. Before every launch it removes a
stale ALSA dmix lock and logs `=== alsa: removed stale dmix lock (…)`; the installer's health check
(a marker ~10 s after the restart, then the log) must not treat that line as a failure.

Updates from HA (`pulsar-update`, entities "Firmware" + "Auto update"; user-facing behaviour in
README, "Обновление из Home Assistant"). What must not break:
- Asset names are constant: README links `releases/latest/download/...`, and the updater downloads
  `releases/download/<tag>/pulsar-<arch>.bin`, `zigbee-coordinator.ybl` and `zigbee-router.ybl`
  (Midi; each skipped when the release has none) and `SHA256SUMS`.
- The latest release is found through the redirect of `https://github.com/<repo>/releases/latest`
  to `/releases/tag/<tag>`, not the REST API: its 60 requests/hour per public IP are exhausted
  behind shared VPN / CGNAT exits.
- `state.conf` `update_attempt=<tag>` is set before the swap and cleared by the new binary; still
  set under another version means `supervise.sh` rolled it back, so auto-update skips that tag.

## Device safety rules

- **Never break root adb access.** Do not touch adbd, `/data/quasar/activate_*` markers or
  `/data/quasar/rootkeeper.sh` (it belongs to the root method): the installer only removes the hook
  lines older versions of it added there (`hookSat` / `hookSSH`) and never creates, deletes or
  otherwise changes the file.
- Do not modify the rootfs (`/system`, `/etc`); everything persistent lives in `/data`. The single
  exception is `/etc/init.d/S99zpulsar`, written by the installer only after checking that the
  rootfs is not protected (`/proc/cmdline` without `verity`/`dm=`, nothing but `control` in
  `/dev/mapper`, `df /etc /system/vendor/quasar` on one filesystem mounted at `/`), with `/`
  remounted rw only for the write and back to ro.
- Protecting a station from Yandex firmware OTA is not pulsar's job (station owner / root method);
  an OTA that replaces the system partition removes `S99zpulsar`, and re-running the installer
  restores it.
- Several devices may share one adb server: always pass `adb -s <serial>`.
- `adb shell` on this firmware is a pty: output has `\r`, is not binary-safe (use push/pull) and
  the exit status is always 0 (see `sh()` in `installer/adb.go`).
- Before replacing the binary, keep the working one as `.prev` (the automatic rollback relies on
  it). Manual escape: `.prev` or `ctl.sh disable` (starts the Yandex stack).
- Do not open ESPHome API connections to a device that Home Assistant is using; test against a
  separate binary on another port with the service stopped (`ctl.sh disable` or kill the
  supervisor). Station tests follow `docs/stations.md` (lock, test instance, volume cap, cleanup).
- No volume changes or test sounds on a live speaker without the owner's consent; tests that play
  audio must cap volume (≤ 0.2 on the Mixer scale, raw ≤ 82).
- `/tmp` is tmpfs (1 GB RAM on the Midi, 237 MB on the Lite 2): remove test files afterwards.

## Yandex libraries (reverse-engineered C API, see `satellite/crates/pulsar-audio/src/engine/`)

From `/system/vendor/quasar/libyandex_evil.so`:

- VQE: `YandexVqeC_Config_CreateFromPreset(<Model::vqe_preset>, &cfg)`,
  `YandexVqeC_Create(cfg, "/system/vendor/quasar/vqe_models", 0, nullptr, &vqe)`,
  `YandexVqeC_Process(vqe, mics, nmic * chunk, spk, nspk * chunk)` with float samples in [-1, 1],
  frame-interleaved (Midi: 4 mics + 4 speaker refs, chunk 3072 @ 96 kHz; Lite 2: 2 + 1, chunk
  1536 @ 48 kHz), `YandexVqeC_GetOutputChannelData(vqe, type, idx, &ptr, &n)` → 512 samples @ 16 kHz.
- Spotter: `spotter_create_multichannel(conf, 16000, n, types, false, &err)`,
  `spotter_submit_raw_data(sp, s16_interleaved, bytes, &err)`. Channel count per type comes from the
  model config. Hit events: 2 and 5.
- Spotter type → VQE output: type 0 = main channel. The wake model (`spotter_models/activation/alisa`)
  needs `VQE_OMNI_WITH_GAIN` (9); the command model (`spotter_models/command`, single channel)
  only works with `VQE_OMNI` (6). Measured with `ww_offline`; re-measure when changing it.
- The command spotter listens whenever a microphone is open (`update_commands`: off only while
  muted), the dialog included. In LISTENING / PROCESSING every command word acts only for the
  2.5 s after the wake word ("Алиса, громче"; `logic::after_wake`); past that window only "стоп" /
  "хватит" act (`logic::command_allowed`), so the request's other words do not act as commands.
  They cancel the dialog like the action button (`cancel_dialog`: cancel earcon,
  `voice_request_stop`, music left playing; a ringing timer is stopped instead), and a stop word
  inside the request ("хватит ли …") cancels it too. Stock maind stops the spotter at the wake
  word and starts it again once Alice is IDLE (Midi 0.351: `stopCommandSpotter reason: external`
  at the wake word, `startCommandSpotter … reason: aliceStateChanged` back in IDLE with nothing
  playing); the 2.5 s window and the stop words in a dialog are pulsar's (stock leaves both to the
  cloud). Stock treats "стоп", "хватит" and "пауза" as one music action ("Остановить / поставить
  на паузу" in Yandex's command reference), so "стоп" pauses. Command words have no arbitration
  between stations, so without the wake word a station acts only on what is its own
  (`logic::command_allowed`): "стоп" / "хватит" everywhere, volume while it sounds (answer,
  announcement, timer ring) or owns music, pause / resume / next / previous only for its own music
  (`logic::owns_music`: playing or loading locally or over Sendspin, or paused / Sendspin stream
  ended less than 2 h ago with the server still connected). A command word recognised before a
  won arbitration ends is held and leaves the answer playing (no dialog starts); spoken "Алиса,
  тише" during an answer can be recognised after it (seen on a Midi: 53 ms), when the barge-in has
  already cut the answer: it then acts in the new dialog's window and cancels that dialog's HA
  pipeline.
- `GetOutputChannelData` always returns no error; an absent output type yields `n == 0`.
- `YandexVqeC_Config_GetOutputChannelCount` takes `(cfg, type, int *out)`; the other `Config_Get*`
  getters return the value directly.

## Zigbee radio (Midi, `satellite/crates/pulsar-zigbee/`)

Telink B91 (RISC-V) on `/dev/ttyS2`, 115200 8N1, no flow control. It came with Yandex's
`gateway.ybl` (`/system/vendor/quasar/zigbee/`; plain Telink image + zlib CRC-32 trailer, header
version at 0x02, manufacturer 0x140A, image type 0x0580) and the network Yandex formed (channel
12). Reset / boot lines: the kernel's `zb_control` driver exports them as
`/sys/devices/platform/zigbee/zb-{reset,boot}/value` (GPIOZ_11 / GPIOZ_9, active low, 1 =
asserted); `/etc/init.d/S30zigbee` of other firmware builds uses raw GPIO numbers that on this
kernel belong to other pins (475 = GPIOX_0, Wi-Fi SDIO): never drive pins by number. After a reset
the application sends 0x8907 within ~0.3 s.

- Framing (stock Telink zbhci, all three firmwares): `0x55 | type BE16 | len BE16 | xor(type, len,
  payload) | payload | 0xAA`. One frame per DMA buffer: every frame is ACKed with 0x8000
  `[type BE16][status][0]` (2 = unknown type) and the next may only follow the ACK.
- Yandex's gateway firmware cannot read or set the network key, PAN ID or extended PAN ID, passes
  APS frames for endpoint 1 only and consumes ZDO responses. pulsar does not run it;
  `ctl.sh disable` hands the radio back to maind, which writes `gateway.ybl` again (its own update
  path, done when the 0x0900 version differs).

### Roles and radio firmware

State `zigbee_role=off|pulsar-coordinator|router` (HA select "Zigbee role": "Off", "Coordinator",
"Router"; `pulsar_zigbee::ROLES`). Default and anything else is off, including `coordinator`
(the Yandex gateway role of old builds): pulsar does not touch the radio, so an install never
writes it without a choice in HA. Choosing another role persists it and restarts the daemon.
The role service (`service.rs`) opens the link and asks for 0x0900 before building the role object
(the deCONZ server listens only then); a firmware other than the role's image
(`/data/pulsar/zigbee-{coordinator,router}.ybl`, which installs and updates bring), or a radio
silent for 10 s (interrupted upload), gets the role's image, as maind does for the gateway. At most
two uploads per start; an image the radio did not take is stored as
`zigbee_flash_failed=<version hex>` and not written on later starts (the role then keeps an older
build of its firmware if the radio has one) until the role changes or another image version
arrives. While an upload runs (~25 s; the 5 s shutdown watchdog would cut it), the HA Restart
button and update installs are refused; an upload cut short anyway (`ctl.sh restart`, power) leaves
a silent radio, written again on the next start. Updates stage the images next to the binary and
replace them together with it.

- `.ybl`: Telink image whose size field (0x18) is the length without the trailer, then zlib CRC-32
  LE. Header version at 0x02; `version >> 24` tells the family: gateway 0x15, pulsar router 0x50,
  pulsar coordinator 0x51. All three answer 0x0900 → 0x8900 `[version BE32][manufacturer
  BE16][image type BE16][size BE32]`.
- Bootloader ("Yandex Bootloader v1.0.02", resident at 0–0x7FFF; the app is linked at 0x20008000):
  boot=1, reset=1, 100 ms, reset=0, 100 ms, boot=0, then `\n` → menu ending in `BL > `; `1` =
  XMODEM upload (128-byte blocks, CRC-16 on 'C', checksum on NAK, last block padded 0x1A then 0x00,
  EOT), `2` = run the app. This is maind's own update path (`ncpmgr`). The flasher refuses a
  non-tty path.
- pulsar firmware (`radio/`, Telink Zigbee SDK @ pinned commit, Apache-2.0): `common/` has the
  board configuration and flash map shared by `router/` and `coordinator/`, and the 0x0900 /
  0x8907 / ACK pieces (`pulsar_hci.c`). UART0 PB2/PB3, `IMAGE_OFFSET` 0x8000, flash protection off
  (the bootloader must be able to write the next image), all other GPIOs left as inputs. Each
  relocates its NV inside the gateway's OTA staging area (0x77000..0xE5FFF) so every network
  survives every switch: router 0xC0000 (factory-reset counter 0xD6000, install code 0xD7000),
  coordinator 0xA8000 (0xBE000, 0xBF000); the gateway's NV is at 0xE6000, MAC/IEEE at 0xFF000 is
  shared. `sdk.patch` makes those addresses and the UART buffer size overridable, BDB's 180 s
  network-wide permit join after forming / joining optional (`BDB_NO_AUTO_PERMIT_JOIN`, both
  images: joining is the coordinator host's decision), and gives groupcasts for groups without a
  member endpoint to the host (`APS_GROUP_HOST_FALLBACK`, coordinator). `build.sh` fetches the SDK
  again when `sdk.patch` changes. Every frame gets exactly one 0x8000 ACK: `zbhciCmdHandler` ACKs
  the router's stock commands (and `hci_uart.c` parse errors), the apps their own. Neither steers
  or forms on its own, nor has the quick-power-cycle factory reset (pulsar resets the radio at
  every start).
- Router zbhci: 0x0002 = network steering (join), 0x0005 = leave (broadcast Leave) + erase its
  network, 0x0045 → 0x8045 network info (24 bytes, channel last); 0x8906 joined (0x8045 layout
  without the channel); 0x8920 `[event][status]`: 1 started (status 0 = on a network), 2
  commissioning failed (BDB status, 3 = no network), 3 left.

### Coordinator

`radio/coordinator/coordinator.c` (frame layouts in its header) is an NCP: the chip runs the stack
and the trust center, the host chooses the network and gets every APS frame. Commands 0x0Axx /
0x8Axx with little-endian fields. 0x0A00 → 0x8A00 state `[state][BDB status][channel][PAN][ext
PAN][IEEE][NWK][update id][key][key seq][frame counter]` (also on every change; the host asks
every 30 s for the frame counter hosts back up); 0x0A01 forms with the host's channel, PAN ID, ext
PAN, key, IEEE, frame counter and update id (only without a network: 0x0A02 erases first; a state
follows the ACK at once, forming or refused); 0x0A03 local permit join; 0x0A04 adds / replaces /
removes (`[ep][0xFF]`) an endpoint (up to 8, 32 clusters); 0x0A05 channel change (broadcast
Mgmt_NWK_Update_req, local switch after 10 s); 0x0A06 energy scan (`rf_startEDScan` per channel,
the radio leaves the network channel meanwhile, the MAC's current channel is kept); 0x0A10 APS data
request with a 1-byte handle (group / NWK / IEEE destination, source endpoint 0 or a registered
one, APS tx options) → exactly one 0x8A10 `[handle][status]`; 0x8A11 every APS indication of a
registered endpoint, every groupcast and every ZDO frame (the stack's ZDO handler is teed, so it
still answers requests itself); 0x8A20 events (device joined / left, joining permitted) for the
log.

Stack facts (prebuilt `libzb_coordinator.a`, checked with the toolchain's objdump):
- formation takes the NIB's PAN ID (`NWK_NIB().panId`, 0xFFFF = random; it moves past a PAN ID it
  hears on the channel; `TL_ZB_ASSOCJOIN_PERMIT_PANID` is ignored);
- `bdb_networkFormationStart` returns the BDB state (FORMATION when started);
- `zb_preConfigNwkKey` also marks the key pre-configured (`ss_ib.preConfiguredKeyType` NWKKEY),
  after which the trust center never transports it and joining devices associate over and over:
  the key goes in with `ss_zdoNwkKeyConfigure` + `ss_zdoUseKey` instead;
- BDB's default `tcRequireKeyExchange` would drop Zigbee 1.2 devices that never update their link
  key (cleared after every BDB init);
- an erase (`zb_resetDevice2FN`) restarts the APS counter and regenerates the factory IEEE; NV
  restores the frame counter + 1024 at boot; the IEEE chosen by the host lives in NV_MODULE_APP
  item 0x40 and is applied after `zb_init`;
- `af_dataCnfHandler` confirms only to endpoint 0 and registered endpoints; `rf_setChannel` also
  writes `g_mac_pib->phyChannelCur`, which the stack saves to NV.

`Coordinator` (`pulsar-zigbee/src/coordinator.rs`) wraps it; ZDO addressed to the coordinator
itself is answered on the host (`local_zdo`: addresses, node / simple descriptors, active
endpoints, permit join, energy scan). `DeconzServer` (`deconz.rs`) poses as ConBee III firmware
0x26780900, protocol 0x0106 (zigpy-deconz then scans energy with ZDO to the coordinator and skips
update_neighbor, watchdog TTL, source routes). It accepts no TCP client until the radio reported
its network and drops the client when the radio restarts (outside a formation): a host told "no
network" forms one, which erases the radio. A new network is formed (erase, then form) only after
the host wrote parameters the radio does not have (PAN ID, extended PAN ID, key, MAC address) and
switched the network state; the network to form is fixed before the erase, so parameters the host
did not write keep their values. deCONZ endpoint slots live in the server (zigbee-herdsman writes
endpoint 0xF2 into slot 1: the ZLL endpoint 2 is then removed). deCONZ tx option 0x02 (NWK
security) is dropped: APS bit 0x02 would disable NWK security. zigbee-herdsman sends NWK_addr_req
with NWK mode but an IEEE address (payload 6 bytes longer than declared): sent as a broadcast.

## Model notes

| | Midi (`cucumber`) | Lite 2 (`plum`) |
|---|---|---|
| SoC / userland | Amlogic A5, aarch64, glibc 2.36, 1 GB RAM; some firmware images run a 32-bit armhf userland (the installer then deploys `build/armhf/`). That image's alsa-lib is versioned; the armhf build reaches its current ABI through `pulsar-alsa` | Amlogic AXG, Cortex-A53, **32-bit armhf** userland, glibc 2.30, kernel 4.9, **237 MB RAM** |
| Storage | `/data` partition (several GB) | `/data` → `/bkupgrade/data` (ubifs, ~81 MB free); rootfs ubifs ro, 87 % full |
| Capture | `hw:0,2`, 8 ch @ 96 kHz: mics 0–3, speaker ref 4–7 | `hw:0,1`, 8 ch @ 48 kHz: mic 0 on ch 0 (= ch 1), mic 1 on ch 2 (= ch 3), speaker feedback on ch 6 (the VQE speaker reference: it leaves ~25–30 dB less echo in the VQE output than ch 4); ch 4/5 only carry a start-up transient. Max buffer 2048 frames (~42 ms) |
| VQE preset | `cucumber`: 4 mics, 4 spk, chunk 3072 | `plum`: 2 mics, 1 spk, 48 kHz, chunk 1536 → 512 @ 16 kHz; outputs 1, 2, 4, 6, 8, 9 |
| Spotters | alisa: types 0, 3, 4; command: type 0 | alisa/yandex: types 0 and 4 (1 ch each); command: type 0. Models in `spotter_models/` are plum-tuned |
| Ring | `/dev/fb0` 24×1 + `/dev/fb1` logo 6×1, BGR | `/dev/fb0` "YNDX LED ring" 3×1, 24 bpp, BGR; trigger `led_ring_fb`; sysfs `plum:<color>:ring1..3`; no logo |
| Clock | `cucumber:white:clock-<seg><digit>`, dimming via chip `current`; no centre dot | `plum:white:clock<SEG><pos>`: A–G × 0–3, colon `DH`+`DL`; `H0..3`, `DEGREE` kept dark (`quasar.cfg` `/leds/clock`); `DM` = centre dot at g height (`Model::clock_center`, extra LED 35 of the stock patterns [inference: position not confirmed by eye]); dimming via per-LED `brightness` 0–255 |
| Keys | `awinic_sar`: 582 action, 115/114 volume | `aw9610x`: same codes |
| Mic mute | input `mute`: `SW_MUTE_DEVICE` | input `gpio-keys`: `SW_MUTE_DEVICE` (+ `KEY_MUTE`) |
| Light sensor | `ltr308als01` (iio) | same |
| Volume | "Master Playback Volume" softvol on `speakers` | same names; `speakers` comes from `/etc/init.d/S45amplifier-settings` (`/tmp/asound-vol.conf`) |
| alsa-lib | 1.2.10, versioned | 1.2.1.2, unversioned |
| Earcons | `common/spotter_*.mp3`, `common/volume_sound.mp3`, `timer.wav`, `ru-RU/vins_error.wav` | same except `ru-RU/playback_error.mp3` instead of `vins_error.wav` |
| Boot | `S97quasar` → `onboot.sh`; then `S99zpulsar` → `boot.sh` | `S99quasar` → `onboot.sh`; then `S99zpulsar` → `boot.sh`; `q stop` available |
| Zigbee | Telink B91 on `/dev/ttyS2`, reset/boot via `zb_control` (see Zigbee radio) | none |

Lite 2 capture: reading and VQE on one thread overrun the 42 ms device buffer, so capture has its
own thread with a FIFO. A few overruns an hour remain (`capture: Broken pipe, recovering`); they
are expected.

Stale dmix lock (both models; mechanism in the `supervise.sh` header): a process killed while
playing leaves every later player silent until a reboot, and a daemon restart alone does not help.
`supervise.sh` clears it before each launch, `tools/station.sh` in `stop`, `test-stop` and
`cleanup`; by hand: `ipcrm -M 0x400 -M 0x401 -S 0x400` when `ipcs -m` shows nattch 0 for both.

Lite 2 LEDs: `led_ring_fb` scales each channel on its way to the LEDs; sysfs `brightness` reads
the framebuffer value × ~0.36 (red), ~0.23 (green), ~0.2 (blue) and caps at 63, so ring colours
checked through sysfs only match the Midi's after that correction (`/dev/fb0` holds the written
B,G,R bytes).

## Conventions

- Rust, one current-thread tokio runtime with a `LocalSet`: every service is an actor started with
  `pulsar_core::actors::spawn_actor`, driven through a `Clone` handle (commands, never blocking)
  and reporting through an unbounded event channel; the controller owns its state and serves all
  channels in one `select!`. Blocking and real-time work (capture, VQE, GLib, the Sendspin sink,
  the radio upload) runs on named `std::thread`s that send events through channels. `unsafe` and
  libc calls live only in `sys.rs` / `ffi.rs` / `gst.rs` and the crash handler. Details, the
  sendspin-cpp link recipe and per-service notes: `docs/architecture.md`.
- Keep the ESPHome API server pending until a valid `HelloRequest`; probes must not displace HA.
- State persisted in `state.conf` (key=value); add new keys with safe defaults. The binary kept as
  `.prev` (possibly an older build) reads the same file after a rollback, so existing keys keep
  their format (`pulsar_core::fmt`: `%g` numbers, `strtof` parsing, C locale). Writes
  (`settings.rs`): `persist()` writes the whole file 1 s after the last change (`StateFile`);
  `persist_now()` writes at once, returns whether the file is on disk and is used for
  `update_attempt` (set when an install starts, cleared after a non-fatal install failure),
  `zigbee_role` / `zigbee_flash_failed` and the `mic_muted` switch. An install is refused (reported
  to HA like a failed one, `update_attempt` restored in memory) when the marker cannot be written.
  Each write is `<path>.tmp`, fsync, rename, fsync of the directory; a failed
  write leaves the old file and stays pending until the next change or flush. `main` flushes a
  pending change once after `run()` returns (inside the teardown watchdog); a crash or SIGKILL
  loses up to 1 s of changes. A new key that must survive a restart, a crash or a binary swap
  right after it changes needs `persist_now()`.
- Comments explain device facts and constraints, not what the code does.
- Floating point: `mul_add` sits exactly where the station builds' rounding requires a fused
  multiply-add; golden tests pin it (`pulsar-audio` `spectrum_golden.rs`, `pulsar-clock`
  `clock_fx.rs` render / levels, `pulsar-hw` `led_fx/tests.rs`, `pulsar-media` `mixer_golden.rs`,
  `pulsar-core` `ambient.rs`); the wake level, `ww_offline` energies, the case matrix, the clock's
  music ranges and the volume number follow the same rule without a test. Do not add or remove
  fusions. armhf builds need `-C target-cpu=cortex-a53` (the Makefile sets it), or `mul_add`
  becomes a libm call.
- ESPHome API: up to 4 clients after a valid hello; states go to all subscribers, the voice
  pipeline to the most recent client still subscribed to it: when that one unsubscribes or
  disconnects, the previous subscriber gets it back (`ApiEvent::VoiceSubscribed(false)` then
  `(true)` on every hand-over), so a probe never leaves HA without the pipeline.
- Requests that can be replaced or cancelled return an id and are answered by an event carrying
  it: `PlayerHandle::play` → `PlayId`, `PlayerEvent::Done { id, ok }` exactly once per `play()`
  (`ok` only if every url played to its end; `stop()` or a replacing `play()` sends `false` for the
  superseded list); `ArbiterHandle::claim` → `Decided { claim_id }`; `voice_request_start` → run id
  of `VoiceStartFailed` / `VoiceAudioOverflow`. The controller ignores stale ids and checks that the
  dialog / announcement an id belongs to is still current (`dialog`, `announce_seq`). Getters of a
  handle right after a command still show the previous state, except the few that the handle
  updates synchronously (listed in `docs/architecture.md`, «Модель исполнения», item 2).
- Voice phases LISTENING/PROCESSING are bounded by a watchdog (30 s / 120 s without a pipeline
  event; ESPHome forwards INTENT_PROGRESS only once, so nothing arrives while the agent thinks);
  any new path into those phases must go through `start_pipeline`.
- Pipeline events carry no run id. Until the dialog's RUN_START every event belongs to the run it
  replaced (HA sends that run's RUN_END first) and is dropped; events while IDLE are dropped;
  `finish_interaction` sends `voice_request_stop` when the dialog's run is still active.
- HA's assist satellite stays "responding" from a run's TTS_START until AnnounceFinished, also
  when nothing plays (`send_tts_report`): sent once when the answer ends or is cut short, for a
  TTS_START that arrives after the dialog ended, and always before the next
  `voice_request_start` (HA applies it by forcing idle). Dropped while an assist announcement's
  completion is pending (its own AnnounceFinished ends the wait; one more would end the
  announcement's wait early); media player announcements send none. It belongs to the voice
  owner's run: dropped when the link is lost, and the server drops one sent after an owner change
  until the next `voice_request_start` (`voice_answer_finished`, like stop and audio), since the
  next subscriber could be waiting for an announcement.
- Volume: `Mixer` maps 0..1 to the stock curve in `Model::volume_map`. `state.conf` has
  `volume2=` (stock curve, authoritative) and `volume=` (same raw level on the pre-curve mapping, for
  binaries rolled back to `.prev`); without `volume2=`, `volume=` is converted once. Mute writes
  softvol raw 0 (still -60 dB, not silence) and also zeroes the channel and Sendspin gains. Local
  changes (keys, voice words) play `common/volume_sound.mp3` on ANNOUNCE (maind's
  SimpleVolumeTonePlayer: every change except its sources "other" and "smart_tts") when the level
  (in whole percent) or mute changed, outside LISTENING / SPEAKING and arbitration and never over
  an announcement, the timer ring or a closing earcon (`logic::want_volume_click`); HA and
  Sendspin changes are silent; music is not ducked under it.
- Wake arbitration (switch "Nearest station answers", `state.conf` `nearest_answers=`, default 1):
  the front-end's processing thread (`wake_level_db` in `pulsar-audio/src/frontend.rs`) reports
  the wake level as the mean of the 8 loudest of the last 47 `VQE_OMNI` frames (post-AEC, no AGC;
  `VQE_OMNI_WITH_GAIN` loses the distance), rounded to 0.1 dB so both sides compare the same
  numbers. A claim is one ASCII datagram `pulsar-wake/1 <name> <wake> <busy> <level>`, sent 3×
  40 ms apart to `255.255.255.255:6054`; the decision comes 250 ms later over claims of the same
  wake word received up to 500 ms before ours. A claim older than the 250 ms window wins outright
  (its station already decided and answers); otherwise busy (speaking, announcing, ringing) beats
  idle, then the higher level, then the larger station name. While arbitrating the ring shows
  LISTENING, mic audio is buffered and sent after `voice_request_start`, a command word is held; a
  losing station blocks voice commands for 2.5 s (cleared by its next wake word or dialog).
  Stations without a voice subscriber do not claim (the wake word shows the usual not-subscribed
  error). HA's `duplicate_wake_up_detected` error (first phrase per 2 s wins across satellites) is
  handled silently: no error ring or sound.
- Clock layers: order in the `pulsar-clock/src/clock.rs` module doc (pairing code > … > time;
  update install keeps it dark from `install_update` until exit). Persisted: `clock_scroll_ms=`,
  `clock_music=`. Player one-shots fire only when the command took effect (`music_pause()` returns
  whether something played; next / previous when Sendspin accepted the transport command).
- LED layers: order in the `pulsar-hw/src/leds.rs` module doc and `active_layer`. Output:
  `(v · ambient dim)²`, case matrix (`led_case_color=`, rows = output R, G, B [inference:
  orientation not confirmed by eye]), × 0.8. Music: Sendspin visualizer frames win while they keep
  coming (`pulsar_core::SENDSPIN_FRESH_MS` 500 ms, shared with the clock), else the
  speaker-reference analysis, which only runs while the visualization is shown.
- Sendspin: `pulsar-sendspin` releases the sink at once on a stream end but reports the end 1 s
  late unless the group state says "stopped", so a server that restarts the stream on seek / skip
  does not flap the HA media state and the music displays.
- deCONZ server limits (`pulsar-zigbee/src/deconz.rs`): a client with more than 64 KB of unread
  replies is disconnected; an APS request gets FAILURE at ≥ 8 pending confirms or > 16 frames
  queued on the link; at most 4 reads of 1024 bytes per wake-up; accepting pauses 500 ms on
  EMFILE / ENFILE / ENOBUFS / ENOMEM; the formation state becomes FORMING after `form()` returns.
- Updater: when every `UpdaterHandle` is dropped (daemon exit), an install in flight is cancelled
  before `--version` and the swap (`pulsar.new` removed; a swap already running completes); the
  controller then clears `update_attempt` at exit unless the install had already succeeded.
- `installer/` (Go) is the only host-side installer. Device commands are busybox-sh strings sent
  through `sh()` (adb shell plus `__rc=` marker; one command ≤ 4 KB on these firmwares).
  The host side uses only the Go stdlib and `golang.org/x/net`, no host tools except adb (plus
  make / `ssh/build.sh` in a checkout). The pure parts (init script `initScript`, rootfs check
  `rootfsGuard` / `rootMountRW`, rootkeeper clean-up `unhookRootkeeper` / `hookActive` /
  `checkRootkeeper`) are covered by `autostart_test.go` and `rootkeeper_test.go`; keep them pure.
