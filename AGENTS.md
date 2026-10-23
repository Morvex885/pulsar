# AGENTS.md

Guide for coding agents working on pulsar. User-facing docs: `README.md` (Russian).

## What this is

A native C++ daemon replacing the Yandex stack (`maind` / `quasar_launcher2`) on rooted Yandex
Stations: Midi (`cucumber`: aarch64, glibc 2.36, 1 GB RAM) and Lite 2 (`plum`: 32-bit armhf,
glibc 2.30, 237 MB RAM). It exposes the speaker to Home Assistant over the ESPHome Native API
(port 6053; plaintext until a Noise key is provisioned, then Noise) and to Music Assistant as a Sendspin player (port 8928); on the Midi
its Zigbee radio runs pulsar's own firmware (`radio/`): a deCONZ-protocol network coordinator for
Zigbee2MQTT / ZHA (port 6638) or a router in another network (e.g. Zigbee2MQTT). Wake word, command
spotting and echo cancellation use the firmware's own proprietary libraries, loaded with `dlopen`
at runtime; they are never copied into the repo. Stations on one LAN arbitrate the wake word over
UDP broadcast (port 6054) so that only the one that heard it loudest answers.

## Layout

| Path | Content |
|---|---|
| `satellite/src/core/` | epoll event loop (`Loop`: fds, timers, cross-thread `post`), logging |
| `satellite/src/esphome/` | ESPHome API server (hand-written protobuf in `proto.h`), mDNS responder |
| `satellite/src/model.*` | per-model hardware description (capture layout, VQE preset, LEDs, clock, inputs, sounds), detected from `/proc/device-tree/compatible` |
| `satellite/src/audio/` | `frontend.*`: capture thread (ALSA → FIFO) + processing thread: Yandex VQE → wake + command spotters → 16 kHz stream; `spectrum.*`: music features of the speaker reference (512-point FFT at 16 kHz → 8 bands, loudness, beats as `MusicFrame`, `music_frame.h`) while the visualization can be shown |
| `satellite/src/media/player.*` | playback through the firmware's GStreamer (`libquasar_gstreamer.so`), channels MUSIC / ANNOUNCE; `Mixer` = ALSA "Master Playback Volume" (softvol of the `speakers` PCM) |
| `satellite/src/sendspin/` | Sendspin player on top of sendspin-cpp, own ALSA sink on the `speakers` dmix; roles player, metadata, controller, visualizer (8 log bands 40 Hz–7.6 kHz, loudness, beats → `MusicFrame`) |
| `satellite/src/zigbee/` | Midi Zigbee radio: zbhci link to the Telink chip (`telink_link.*`), coordinator on pulsar's firmware + ZDO to the coordinator itself (`coordinator.*`), deCONZ-protocol TCP server for Zigbee2MQTT / ZHA (`deconz_server.*`), router control (`router.*`), `.ybl` parsing + XMODEM upload through the radio's bootloader (`flasher.*`), role / firmware orchestration (`service.*`), byte order helpers (`bytes.h`) |
| `satellite/src/update/` | HA "Firmware" update entity backend: HTTPS on the firmware's OpenSSL (`https.*`, dlopen), GitHub release check / download / verify / swap (`updater.*`) |
| `satellite/src/hw/` | LED ring / logo (framebuffers behind ledfb triggers; `leds.cpp`: output, case matrix, layer compositor, fades), pure rendering of voice states / one-shots / effects / music styles (`led_fx.*`), clock (sysfs segments), input (touch keys, `SW_MUTE_DEVICE`), light sensor (`ltr308als01`) |
| `satellite/src/controller.*` | all satellite logic: voice pipeline state machine, entities, local commands, persistence |
| `satellite/src/arbiter.*` | `WakeArbiter`: wake word claims between stations (UDP broadcast :6054, 250 ms window), best claim answers |
| `satellite/deploy/` | `dist.sh` (release archives, host side); `boot.sh`, `supervise.sh`, `ctl.sh` (device side) |
| `installer/` | `pulsar-install` (Go): station discovery (mDNS legacy-unicast query / `--scan` port scan), adb (downloads platform-tools 37.0.1 if missing), install (incl. `/etc/init.d/S99zpulsar`), health check |
| `satellite/tools/` | probes: `hw_probe`, `player_probe`, `ww_offline` (offline spotter test on a WAV), `api_selftest`, `update_probe` (HTTPS client, release check, install into a scratch dir), `zigbee_probe` (Zigbee coordinator, either firmware, + deCONZ server on another port, without the daemon; `flash <image>` writes a radio image), `alsa_abi_probe` (ALSA hw-params ABI on the `null` PCM, no sound) |
| `satellite/third_party/` | `fetch.sh` (pinned deps), `sysroot.sh` (alsa-lib link sysroot), CMake glue, zig toolchain wrappers |
| `ssh/` | `build.sh` (static dropbear + sftp-server), `start.sh` (device side) |
| `radio/` | Zigbee firmware for the Midi radio on the Telink Zigbee SDK: `build.sh` (pinned SDK + toolchain, `sdk.patch`), `Makefile`, `coordinator/` and `router/` (apps), `ybl.c` (host tool: binary → `.ybl`) |

Git-ignored and never to be published: `backup/` (full device backups), `dump/` (firmware binaries,
logs), `re/` (Ghidra projects and decompiled Yandex code), `ref/` (upstream checkouts).

## Build

```sh
make -C satellite -j"$(nproc)"                       # both: build/{aarch64,armhf}/pulsar
make -C satellite ARCH=armhf                          # one architecture
make -C satellite ARCH=armhf build/armhf/tools/ww_offline   # any tool in satellite/tools/
ssh/build.sh                                         # → ssh/out/{aarch64,armhf}/{dropbearmulti,sftp-server}
radio/build.sh                                       # → radio/out/zigbee-{coordinator,router}.ybl (needs libc6:i386, unzip)
go -C installer test ./...                           # installer unit tests
go -C installer run . [args]                         # installer from a checkout (builds the daemon first)
```

Host tools: **zig 0.14.1** (`ZIG=/path/to/zig` if not in PATH; the Makefile rejects ≤ 0.13, whose
armhf C++ release builds crash before `main`), make, git, curl, cmake ≥ 3.16, ninja, patch; Go 1.27+
(installer). The
first build fetches pinned dependencies into `satellite/third_party/src/` and builds an ALSA
sysroot per architecture into `satellite/build/<arch>/sysroot/` (Midi: versioned like the
firmware's alsa-lib; Lite 2: unversioned, exporting only `snd_*`, otherwise the daemon would import
zig's compiler-rt helpers from libasound). Outputs per arch: `pulsar` (stripped, the
one deployed) and `pulsar.debug` (unstripped, line tables) for `addr2line -f -e` on the
crash report `main.cpp` writes to the log (non-PIE: addresses are absolute; armhf reports pc/lr only:
glibc's `backtrace()` there runs libgcc_s's unwinder into the binary's static zig personality
routine and segfaults). The version is
`git describe --tags --always --dirty` (`--version`, HA DeviceInfo and the "Version" entity). Host-side
tests exist only for the installer (`go -C installer test ./...`); the daemon is verified on the
device.

CI (`.github/workflows/build.yml`) builds the daemon and every tool for both architectures and the
radio firmware images (job `radio`) on each pull request and push to `main`; the binaries are
attached to the run as artifacts. Job `installer`
vets the installer for linux/windows/darwin, runs its tests and cross-builds it for windows/amd64,
darwin/{arm64,amd64} and linux/{amd64,arm64}. Pushing a tag `v*` additionally builds dropbear (job
`ssh`, `ssh/build.sh`) and publishes a GitHub release with one archive per host OS,
`pulsar-{windows-amd64.zip,macos-{arm64,amd64}.tar.gz,linux-{amd64,arm64}.tar.gz}` (assembled by
`satellite/deploy/dist.sh`: `pulsar-install`, both daemon binaries, `radio/out/zigbee-*.ybl`,
dropbear and the device scripts in the repo layout), plus `pulsar-<arch>.bin`, `pulsar-<arch>.debug`,
`zigbee-coordinator.ybl`, `zigbee-router.ybl` and `SHA256SUMS`.
Asset names are constant: README links `releases/latest/download/...`, and the daemon's updater
downloads `releases/download/<tag>/pulsar-<arch>.bin`, `zigbee-coordinator.ybl` and
`zigbee-router.ybl` (Midi; each skipped when the release has none) and `SHA256SUMS` (keep the
names). Only
stable releases are offered (`releases/latest`); the tag must equal the binary's
`git describe` (the build job checks out the tag, so it does).

## Branches

`main` is the only long-lived branch and is protected by a repository ruleset: no direct pushes,
force pushes or deletion; changes land through a pull request with green `build (aarch64)` and
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
(`detect_model()`, exits on unknown hardware) and by the installer (`archOf()`: codename → binary,
`modelName()`, in `installer/probe.go`). Adding a model: hardware study first (ALSA capture
layout, VQE preset, spotter channels, LEDs, clock, inputs, earcons), then a `Model` in
`src/model.cpp` and `archOf()` / `modelName()` entries.

Manual binary update (e.g. over ssh):

```sh
scp satellite/build/<arch>/pulsar root@<ip>:/data/pulsar/pulsar.new
ssh root@<ip> 'cd /data/pulsar && cp pulsar pulsar.prev &&
  mv pulsar.new pulsar && ./ctl.sh restart'
```

Device layout: `/data/pulsar/{pulsar,.prev,.bad,boot.sh,supervise.sh,
ctl.sh,state.conf,api_key,enabled,sendspin/,zigbee-coordinator.ybl,zigbee-router.ybl}`. Outside /data:
`/etc/init.d/S99zpulsar` (autostart, written by the installer). `api_key`: ESPHome Noise PSK (base64 of 32 bytes,
0600), written by HA's auto-provisioning or `ctl.sh api-key`; absent = plaintext (Noise with the
all-zeros key is then accepted so HA can provision); corrupt = API fails closed. `.bad`: binary
replaced by the crash-loop rollback. Log: `/tmp/pulsar.log` (`ctl.sh log N`; rotated
at 1 MB by `supervise.sh`, one `.1` kept). Boot chain: busybox init → `rcS` (runs
`/etc/init.d/S??*` in order with `start`, waiting for each) → `S97quasar` (Midi) / `S99quasar`
(Lite 2) → `onboot.sh` (adbd via `activate_adb.sh check`, quasar in background; returns); then
`S99zpulsar` (sorts after `S99quasar`; `S99pulsar` would not) → `boot.sh` in background (once per
boot: `/tmp/pulsar-boot.lock`) → waits for quasar, `q stop` → `supervise.sh` restart loop while
`enabled` exists. `ctl.sh reload` restarts the supervisor itself
(needed after replacing `supervise.sh`; the installer does it when the script changed).

Updates from HA (`src/update/`, entities "Firmware" + "Auto update"): the daemon checks
`https://github.com/<repo>/releases/latest` (its redirect to `/releases/tag/<tag>`; the REST API
is not used: its 60 requests/hour per public IP are exhausted behind shared VPN/CGNAT exits, seen
on the test stations) a minute after start and every 6 h. Install: download over HTTPS (busybox
`wget` has no TLS, there is no curl; `libssl.so.3` on the Midi, `libssl.so.1.1` on the Lite 2,
CA bundle `/etc/ssl/certs/ca-certificates.crt`), sha256 against `SHA256SUMS`, `pulsar.new
--version` must print `pulsar <tag>`, current binary copied to `.prev`, rename, exit 0.
`state.conf` `update_attempt=<tag>` is set before the swap and cleared by the new binary; still
set under another version = rolled back by `supervise.sh`, so auto-update skips that tag.
Automatic installs need a parseable current version (`vX.Y.Z[-N-g<hash>]`) and an idle speaker.

Supervisor contract (`supervise.sh` header has the details): daemon exit 0 = deliberate restart
(SIGTERM from `ctl.sh`, the HA "Restart" button via `on_restart`), restarted after 1 s and not
counted as a crash. Anything else is a crash; backoff 3 s doubling to 30 s; 5 consecutive crashes
each after < 300 s of uptime → swap in `.prev` (bad one kept as `.bad`); if that crash-loops too →
remove `enabled` and start the Yandex stack. The installer additionally checks the log after a
marker ~10 s post-restart and offers to restore `.prev`. A pre-Noise binary rolled back onto a
device whose HA already holds a key cannot talk to HA until the key is removed there.

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
  separate binary on another port with the service stopped (`ctl.sh disable` or kill the supervisor).
- No volume changes or test sounds on a live speaker without the owner's consent; tests that play
  audio must cap volume.
- `/tmp` is tmpfs (1 GB RAM on the Midi, 237 MB on the Lite 2): remove test files afterwards.

## Yandex libraries (reverse-engineered C API, see `src/audio/frontend.cpp`)

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
- The command spotter only runs while something plays, during TTS/announcements/timer ring, or
  2.5 s after the wake word (as `maind` does), to avoid false positives from conversation.
- `GetOutputChannelData` always returns no error; an absent output type yields `n == 0`.
- `YandexVqeC_Config_GetOutputChannelCount` takes `(cfg, type, int *out)`; the other `Config_Get*`
  getters return the value directly.

## Zigbee radio (Midi, `src/zigbee/`)

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
  APS frames for endpoint 1 only and consumes ZDO responses; pulsar drove it as a ZHA coordinator
  until its own firmware existed (git history: `src/zigbee/coordinator.cpp` before 3251a71).
  pulsar no longer runs it; `ctl.sh disable` hands the radio back to maind, which writes
  `gateway.ybl` again (its own update path, done when the 0x0900 version differs).

### Roles and radio firmware

State `zigbee_role=off|pulsar-coordinator|router` (HA select "Zigbee role": "Off", "Coordinator",
"Router"; `ZigbeeService::kRoles`). Default and anything else is off, including `coordinator`
(the Yandex gateway role pulsar had before): pulsar does not touch the radio, so an install never
writes it without a choice in HA. Choosing another role persists it and restarts the daemon.
`ZigbeeService` opens the link and asks for 0x0900 before building the role object (the deCONZ
server listens only then); a firmware other than the role's image
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

`ZigbeeCoordinator` (`coordinator.*`) wraps it; ZDO addressed to the coordinator itself is answered
on the host (`local_zdo`: addresses, node / simple descriptors, active endpoints, permit join,
energy scan). `DeconzServer` poses as ConBee III firmware 0x26780900, protocol 0x0106 (zigpy-deconz
then scans energy with ZDO to the coordinator and skips update_neighbor, watchdog TTL, source
routes). It accepts no TCP client until the radio reported its network and drops the client when
the radio restarts (outside a formation): a host told "no network" forms one, which erases the
radio. A new network is formed (erase, then form) only after the host wrote parameters the radio
does not have (PAN ID, extended PAN ID, key, MAC address) and switched the network state; the
network to form is fixed before the erase, so parameters the host did not write keep their
values. deCONZ endpoint slots live in the server (zigbee-herdsman writes endpoint 0xF2 into slot 1:
the ZLL endpoint 2 is then removed). deCONZ tx option 0x02 (NWK security) is dropped: APS bit
0x02 would disable NWK security. zigbee-herdsman sends NWK_addr_req with NWK mode but an IEEE
address (payload 6 bytes longer than declared): sent as a broadcast.

Verified on a Midi (firmware 0.351): router image written (~20 s), joined a Zigbee2MQTT network in
~4 s and interviewed there, left (Z2M dropped it); new router builds written automatically at start
kept the node in its network (same NWK address). Coordinator with zigpy-deconz: formation and
`write_network_info` with chosen PAN ID / ext PAN / key / channel / IEEE / frame counter / update
id read back, network and IEEE kept over radio restarts and re-flashing, coordinator interview,
energy scan, permit join; with zigbee-herdsman 11.1 (Zigbee2MQTT's stack, deconz adapter over TCP
to the daemon): first start forms its configured network, coordinator backup written, later starts
resume without re-forming, permit join, an Aqara WXKG11LM button (Zigbee 1.2 end device) joined,
was interviewed and reported presses, also after the daemon updated the coordinator image. The
button left once ~15 s after its first join (BDB's `tcNodeJoinTimeout` is 15 s;
`ss_ib.tcPolicy.updateTCLKrequired` may still be enforced) and rejoined for good; not yet
explained. Off-device: XMODEM upload against an emulated bootloader (byte-identical). Not yet
verified: groupcasts in either direction, channel change, routers joining through other routers,
larger networks, routing load, migration from another adapter (per-device TC link keys are not
carried over).

## Model notes

| | Midi (`cucumber`) | Lite 2 (`plum`) |
|---|---|---|
| SoC / userland | Amlogic A5, aarch64, glibc 2.36, 1 GB RAM; some firmware images run a 32-bit armhf userland with a versioned alsa-lib (the armhf build reaches its current ABI through `audio/alsa_abi.h`) | Amlogic AXG, Cortex-A53, **32-bit armhf** userland, glibc 2.30, kernel 4.9, **237 MB RAM** |
| Storage | `/data` partition (several GB) | `/data` → `/bkupgrade/data` (ubifs, ~81 MB free); rootfs ubifs ro, 87 % full |
| Capture | `hw:0,2`, 8 ch @ 96 kHz: mics 0–3, speaker ref 4–7 | `hw:0,1`, 8 ch @ 48 kHz: mic 0 on ch 0 (= ch 1), mic 1 on ch 2 (= ch 3), speaker feedback on ch 6; ch 4/5 only carry a start-up transient. Max buffer 2048 frames (~42 ms) |
| VQE preset | `cucumber`: 4 mics, 4 spk, chunk 3072 | `plum`: 2 mics, 1 spk, 48 kHz, chunk 1536 → 512 @ 16 kHz; outputs 1, 2, 4, 6, 8, 9 |
| Spotters | alisa: types 0, 3, 4; command: type 0 | alisa/yandex: types 0 and 4 (1 ch each); command: type 0. Models in `spotter_models/` are plum-tuned |
| Ring | `/dev/fb0` 24×1 + `/dev/fb1` logo 6×1, BGR | `/dev/fb0` "YNDX LED ring" 3×1, 24 bpp, BGR; trigger `led_ring_fb`; sysfs `plum:<color>:ring1..3`; no logo |
| Clock | `cucumber:white:clock-<seg><digit>`, dimming via chip `current` | `plum:white:clock<SEG><pos>`: A–G × 0–3, colon `DH`+`DL`; `H0..3`, `DM`, `DEGREE` kept dark (`quasar.cfg` `/leds/clock`); dimming via per-LED `brightness` 0–255 |
| Keys | `awinic_sar`: 582 action, 115/114 volume | `aw9610x`: same codes |
| Mic mute | input `mute`: `SW_MUTE_DEVICE` | input `gpio-keys`: `SW_MUTE_DEVICE` (+ `KEY_MUTE`) |
| Light sensor | `ltr308als01` (iio) | same |
| Volume | "Master Playback Volume" softvol on `speakers` | same names; `speakers` comes from `/etc/init.d/S45amplifier-settings` (`/tmp/asound-vol.conf`) |
| alsa-lib | 1.2.10, versioned | 1.2.1.2, unversioned |
| Earcons | `common/spotter_*.mp3`, `timer.wav`, `ru-RU/vins_error.wav` | same except `ru-RU/playback_error.mp3` instead of `vins_error.wav` |
| Boot | `S97quasar` → `onboot.sh`; then `S99zpulsar` → `boot.sh` | `S99quasar` → `onboot.sh`; then `S99zpulsar` → `boot.sh`; `q stop` available |
| Zigbee | Telink B91 on `/dev/ttyS2`, reset/boot via `zb_control` (see Zigbee radio) | none |

Lite 2 measurements: echo test (speech played through `speakers`, raw capture → `ww_offline`):
speaker reference ch 6 gives ~25–30 dB lower VQE output than ch 4 and no false "алиса" hit from the
played speech. Daemon: ~23 MB RSS, ~7 % CPU idle-listening. Capture overran the 42 ms device
buffer when reading and VQE shared a thread; hence the separate capture thread with a FIFO.

Lite 2 LEDs: `led_ring_fb` scales each channel on its way to the LEDs; sysfs `brightness` reads
the framebuffer value × ~0.36 (red), ~0.23 (green), ~0.2 (blue), so ring colours checked through
sysfs only match the Midi's after that correction (`/dev/fb0` holds the written B,G,R bytes).

Not yet verified on the Lite 2: wake word → HA pipeline → TTS end to end, Sendspin playback, LED
animations by eye. GStreamer playback of an HTTP WAV through the media player works (heard on the
speaker reference by the music analysis).

## Conventions

- C++20, single-threaded controller: worker threads (capture, GStreamer, Sendspin) hand results to
  the loop with `Loop::post`.
- Keep the ESPHome API server pending until a valid `HelloRequest`; probes must not displace HA.
- State persisted in `state.conf` (key=value); add new keys with safe defaults.
- Comments explain device facts and constraints, not what the code does.
- ESPHome API: up to 4 clients after a valid hello; states go to all subscribers, the voice
  pipeline to the most recent client still subscribed to it: when that one unsubscribes or
  disconnects, the previous subscriber gets it back (`on_voice_subscribed(false)` then `(true)` on
  every hand-over), so a probe never leaves HA without the pipeline.
- `Player::play` done callback: called exactly once per `play()` on the Loop; `true` only if every
  url played to its end; `stop()` or a replacing `play()` delivers `false` for the superseded list
  (queued by that call, so it runs after the caller returns). Callers must check that the dialog /
  announcement they belong to is still current (`dialog_`, `announce_seq_` in the controller).
- Voice phases LISTENING/PROCESSING are bounded by a watchdog (30 s / 120 s without a pipeline
  event; ESPHome forwards INTENT_PROGRESS only once, so nothing arrives while the agent thinks);
  any new path into those phases must go through `start_pipeline`.
- Pipeline events carry no run id. Until the dialog's RUN_START every event belongs to the run it
  replaced (HA sends that run's RUN_END first) and is dropped; events while IDLE are dropped;
  `finish_interaction` sends `voice_request_stop` when the dialog's run is still active.
- Volume: `Mixer` maps 0..1 to the stock curve in `Model::volume_map`. `state.conf` has
  `volume2=` (stock curve, authoritative) and `volume=` (same raw level on the pre-curve mapping, for
  binaries rolled back to `.prev`); without `volume2=`, `volume=` is converted once. Mute writes
  softvol raw 0 (still -60 dB, not silence) and also zeroes the channel and Sendspin gains.
- Wake arbitration (switch "Nearest station answers", `state.conf` `nearest_answers=`, default 1):
  `AudioFrontend` reports the wake level as the mean of the 8 loudest of the last 47 `VQE_OMNI`
  frames (post-AEC, no AGC; `VQE_OMNI_WITH_GAIN` loses the distance), rounded to 0.1 dB so both
  sides compare the same numbers. A claim is one ASCII datagram `pulsar-wake/1 <name> <wake>
  <busy> <level>`, sent 3× 40 ms apart to `255.255.255.255:6054`; the decision comes 250 ms later
  over claims of the same wake word received up to 500 ms before ours. A claim older than the
  250 ms window wins outright (its station already decided and answers); otherwise busy (speaking,
  announcing, ringing) beats idle, then the higher level, then the larger station name. While
  arbitrating the ring shows LISTENING, mic audio is buffered and sent after `voice_request_start`,
  a command word is held; a losing station blocks voice commands for 2.5 s (cleared by its next
  wake word or dialog). Stations without a voice subscriber do not claim (the wake word shows the
  usual not-subscribed error). HA's `duplicate_wake_up_detected` error (first phrase per 2 s wins
  across satellites) is handled silently: no error ring or sound. Not yet verified on stations:
  level difference near/far (expected ≥ 6 dB at 0.5 m vs 3 m; otherwise switch the level source to
  `RAW_MAIN_MIC_SYNC`), Midi vs Lite 2 level offset, broadcast delivery through the firmware's
  iptables and the access point.
- Clock layers (`Clock::Impl::refresh`): pairing code (ignores the "Clock display" switch) > volume
  flash > timer > temporary layer (`clock_show_text`/`clock_show_face` actions) > Text «Clock text» /
  Select «Clock face» > time. Only the scroll speed is persisted (`clock_scroll_ms=`). Moving
  content uses its own one-shot frame timer; the 5 s full segment rewrite counts 250 ms ticks only.
- LED layers (`Leds::Impl::active_layer`, first active wins): volume overlay (1.5 s) > voice state
  (ERROR runs its 3.54 s to the end, TIMER_RING; LISTENING / THINKING / SPEAKING only with
  `voice_indication=1`) > one-shots (mute sweep 600 ms over the timer-started countdown 15 s,
  which goes on underneath) > `led_show` > Lite 2 mute (steady dim red) > Light «Ring» > music
  visualization (`music_viz=` ≠ Off and MUSIC channel playing / loading or a Sendspin stream) >
  off. The Midi logo shows the voice state, else red while muted, else the ring's average. Layer
  changes crossfade 250 ms, Light «Ring» changes over their transition (ESPHome default 1 s,
  capped at 60 s). Voice states, one-shots and volume use the stock
  patterns' colours and timings, re-drawn procedurally (`led_fx.cpp`); `fx::stock(raw, k)` gives
  the stock raw value × k after the gamma-2 output. Output: `(v · ambient dim)²`, case matrix
  (`led_case_color=`, rows = output R, G, B [inference: orientation not confirmed by eye]), × 0.8.
  Effects and music move on a phase scaled by `led_speed=` (0.25–4). Music: Sendspin visualizer
  frames win while they keep coming (300 ms), else the speaker-reference analysis, which only runs
  while the visualization is shown (the light off). Not yet verified: Music Assistant actually
  sending visualizer frames (sendspin-cli 8.0.0 with aiosendspin 9.1.1 fails the device's Noise
  handshake, `psk_category` missing; with aiosendspin 10.2.1 forced it connects but never starts a
  stream).
- `installer/` (Go) is the only host-side installer. Device commands are busybox-sh strings sent
  through `sh()` (adb shell plus `__rc=` marker; one command ≤ 4 KB on these firmwares).
  The host side uses only the Go stdlib and `golang.org/x/net`, no host tools except adb (plus
  make / `ssh/build.sh` in a checkout). The pure parts (init script `initScript`, rootfs check
  `rootfsGuard` / `rootMountRW`, rootkeeper clean-up `unhookRootkeeper` / `hookActive` /
  `checkRootkeeper`) are covered by `autostart_test.go` and `rootkeeper_test.go`; keep them pure.
