# AGENTS.md

Guide for coding agents working on pulsar. User-facing docs: `README.md` (Russian).

## What this is

A native C++ daemon replacing the Yandex stack (`maind` / `quasar_launcher2`) on rooted Yandex
Stations: Midi (`cucumber`: aarch64, glibc 2.36, 1 GB RAM) and Lite 2 (`plum`: 32-bit armhf,
glibc 2.30, 237 MB RAM). It exposes the speaker to Home Assistant over the ESPHome Native API
(port 6053; plaintext until a Noise key is provisioned, then Noise) and to Music Assistant as a Sendspin player (port 8928). Wake word, command
spotting and echo cancellation use the firmware's own proprietary libraries, loaded with `dlopen`
at runtime; they are never copied into the repo.

## Layout

| Path | Content |
|---|---|
| `satellite/src/core/` | epoll event loop (`Loop`: fds, timers, cross-thread `post`), logging |
| `satellite/src/esphome/` | ESPHome API server (hand-written protobuf in `proto.h`), mDNS responder |
| `satellite/src/model.*` | per-model hardware description (capture layout, VQE preset, LEDs, clock, inputs, sounds), detected from `/proc/device-tree/compatible` |
| `satellite/src/audio/frontend.*` | capture thread (ALSA → FIFO) + processing thread: Yandex VQE → wake + command spotters → 16 kHz stream |
| `satellite/src/media/player.*` | playback through the firmware's GStreamer (`libquasar_gstreamer.so`), channels MUSIC / ANNOUNCE; `Mixer` = ALSA "Master Playback Volume" (softvol of the `speakers` PCM) |
| `satellite/src/sendspin/` | Sendspin player on top of sendspin-cpp, own ALSA sink on the `speakers` dmix |
| `satellite/src/update/` | HA "Firmware" update entity backend: HTTPS on the firmware's OpenSSL (`https.*`, dlopen), GitHub release check / download / verify / swap (`updater.*`) |
| `satellite/src/hw/` | LED ring / logo (framebuffers behind ledfb triggers), clock (sysfs segments), input (touch keys, `SW_MUTE_DEVICE`), light sensor (`ltr308als01`) |
| `satellite/src/controller.*` | all satellite logic: voice pipeline state machine, entities, local commands, persistence |
| `satellite/deploy/` | `dist.sh` (release archives, host side); `boot.sh`, `supervise.sh`, `ctl.sh` (device side) |
| `installer/` | `pulsar-install` (Go): station discovery (mDNS legacy-unicast query / `--scan` port scan), adb (downloads platform-tools 37.0.1 if missing), install, health check |
| `satellite/tools/` | probes: `hw_probe`, `player_probe`, `ww_offline` (offline spotter test on a WAV), `api_selftest`, `update_probe` (HTTPS client, release check, install into a scratch dir) |
| `satellite/third_party/` | `fetch.sh` (pinned deps), `sysroot.sh` (alsa-lib link sysroot), CMake glue, zig toolchain wrappers |
| `ssh/` | `build.sh` (static dropbear + sftp-server), `start.sh` (device side) |

Git-ignored and never to be published: `backup/` (full device backups), `dump/` (firmware binaries,
logs), `re/` (Ghidra projects and decompiled Yandex code), `ref/` (upstream checkouts).

## Build

```sh
make -C satellite -j"$(nproc)"                       # both: build/{aarch64,armhf}/pulsar
make -C satellite ARCH=armhf                          # one architecture
make -C satellite ARCH=armhf build/armhf/tools/ww_offline   # any tool in satellite/tools/
ssh/build.sh                                         # → ssh/out/{aarch64,armhf}/{dropbearmulti,sftp-server}
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

CI (`.github/workflows/build.yml`) builds the daemon and every tool for both architectures on each
pull request and push to `main`; the binaries are attached to the run as artifacts. Job `installer`
vets the installer for linux/windows/darwin, runs its tests and cross-builds it for windows/amd64,
darwin/{arm64,amd64} and linux/{amd64,arm64}. Pushing a tag `v*` additionally builds dropbear (job
`ssh`, `ssh/build.sh`) and publishes a GitHub release with one archive per host OS,
`pulsar-{windows-amd64.zip,macos-{arm64,amd64}.tar.gz,linux-{amd64,arm64}.tar.gz}` (assembled by
`satellite/deploy/dist.sh`: `pulsar-install`, both daemon binaries, dropbear and the device
scripts in the repo layout), plus `pulsar-<arch>.bin`, `pulsar-<arch>.debug` and `SHA256SUMS`.
Asset names are constant: README links `releases/latest/download/...`, and the daemon's updater
downloads `releases/download/<tag>/pulsar-<arch>.bin` and `SHA256SUMS` (keep both names). Only
stable releases are offered (`releases/latest`); the tag must equal the binary's
`git describe` (the build job checks out the tag, so it does).

## Branches

`main` is the only long-lived branch and is protected by a repository ruleset: no direct pushes,
force pushes or deletion; changes land through a pull request with green `build (aarch64)` and
`build (armhf)` checks, squash or rebase only (linear history). Work on a short-lived branch
(`feat/…`, `fix/…`, `docs/…`), open a PR, merge; the branch is deleted on merge. Release tags `v*`
cannot be moved or deleted. Nothing personal goes into the repository: no real IPs, MACs, serials,
Wi-Fi names, tokens, device logs or names/e-mails in commits (`192.168.1.50` is the documentation
placeholder).

## Deploy

```sh
go -C installer run . --list         # discover stations (mDNS; --scan = adb port scan of the /24) + model probe
go -C installer run . <ip>[:port]    # build if needed, copy binary (+ .prev), add boot hook, restart
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
ctl.sh,state.conf,api_key,enabled,sendspin/}`. `api_key`: ESPHome Noise PSK (base64 of 32 bytes,
0600), written by HA's auto-provisioning or `ctl.sh api-key`; absent = plaintext (Noise with the
all-zeros key is then accepted so HA can provision); corrupt = API fails closed. `.bad`: binary
replaced by the crash-loop rollback. Log: `/tmp/pulsar.log` (`ctl.sh log N`; rotated
at 1 MB by `supervise.sh`, one `.1` kept). Boot chain:
`onboot.sh` (firmware) → `/data/quasar/rootkeeper.sh` → `boot.sh` → waits for quasar, `q stop` →
`supervise.sh` restart loop while `enabled` exists. `ctl.sh reload` restarts the supervisor itself
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

- **Never break root adb access.** Do not touch adbd, `/data/quasar/activate_*` markers, or the
  existing contents of `/data/quasar/rootkeeper.sh`. It is shared with other tooling: only insert
  hook lines, and only before its endless `while` loop (code after the loop never runs).
- Do not modify the rootfs (`/system`, `/etc`); everything persistent lives in `/data`.
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

## Model notes

| | Midi (`cucumber`) | Lite 2 (`plum`) |
|---|---|---|
| SoC / userland | Amlogic A5, aarch64, glibc 2.36, 1 GB RAM | Amlogic AXG, Cortex-A53, **32-bit armhf** userland, glibc 2.30, kernel 4.9, **237 MB RAM** |
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
| Boot hook | `onboot.sh` → `/data/quasar/rootkeeper.sh` | same (line 67); `q stop` available |

Lite 2 measurements: echo test (speech played through `speakers`, raw capture → `ww_offline`):
speaker reference ch 6 gives ~25–30 dB lower VQE output than ch 4 and no false "алиса" hit from the
played speech. Daemon: ~23 MB RSS, ~7 % CPU idle-listening. Capture overran the 42 ms device
buffer when reading and VQE shared a thread; hence the separate capture thread with a FIFO.

Not yet verified on the Lite 2: wake word → HA pipeline → TTS end to end, GStreamer playback
through the 32-bit `libquasar_gstreamer.so`, Sendspin playback, LED animations by eye.

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
- Clock layers (`Clock::Impl::refresh`): pairing code (ignores the "Clock display" switch) > volume
  flash > timer > temporary layer (`clock_show_text`/`clock_show_face` actions) > Text «Clock text» /
  Select «Clock face» > time. Only the scroll speed is persisted (`clock_scroll_ms=`). Moving
  content uses its own one-shot frame timer; the 5 s full segment rewrite counts 250 ms ticks only.
- `installer/` (Go) is the only host-side installer. Device commands are busybox-sh strings sent
  through `sh()` (adb shell plus `__rc=` marker; one command ≤ 4 KB on these firmwares).
  The host side uses only the Go stdlib and `golang.org/x/net`, no host tools except adb (plus
  make / `ssh/build.sh` in a checkout). The rootkeeper edit (`planRootkeeper`) is covered by
  `rootkeeper_test.go`; keep it pure.
