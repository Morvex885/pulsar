# Hardware and firmware reference

Hardware observations and reverse-engineered firmware interfaces. Paths are relative to the
repository root unless they name a station filesystem path. Read [station safety rules](../AGENTS.md#device-safety)
and the [test protocol](stations.md) before working on a device. Mini 3 details: [mini3.md](mini3.md).

## Yandex libraries

C API: `satellite/crates/pulsar-audio/src/engine/`.

From `/system/vendor/quasar/libyandex_evil.so`:

- VQE: `YandexVqeC_Config_CreateFromPreset(<Model::vqe_preset>, &cfg)`,
  `YandexVqeC_Create(cfg, "/system/vendor/quasar/vqe_models", 0, nullptr, &vqe)`,
  `YandexVqeC_Process(vqe, mics, nmic * chunk, spk, nspk * chunk)` with float samples in [-1, 1],
  frame-interleaved (Midi: 4 mics + 4 speaker refs, chunk 3072 @ 96 kHz; Lite 2: 2 + 1, chunk
  1536 @ 48 kHz; Mini 2: 4 + 2 @ 48 kHz; Mini 3: 3 + 1, chunk 768 @ 48 kHz; Mini: 4 + 2, chunk
  768 @ 48 kHz), `YandexVqeC_GetOutputChannelData(vqe, type, idx, &ptr, &n)` → 512 samples @ 16 kHz
  (256 on the Mini 3 and Mini).
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

## Model notes

| | Midi (`cucumber`) | Lite 2 (`plum`) | Mini 2 (`station-mini2`) | Mini (`yandexmini`, `axg_s420_v03`) |
|---|---|---|---|---|
| SoC / userland | Amlogic A5, aarch64, glibc 2.36, 1 GB RAM; some firmware images run a 32-bit armhf userland (the installer then deploys `build/armhf/`). That image's alsa-lib is versioned; the armhf build reaches its current ABI through `pulsar-alsa` | Amlogic AXG, Cortex-A53, **32-bit armhf** userland, glibc 2.30 (runs the 2.23 armhf build), kernel 4.9, **237 MB RAM** | Amlogic AXG, aarch64 kernel with the same **32-bit armhf** userland, glibc 2.30 (runs the 2.23 armhf build), kernel 4.9.68, 238 MB RAM | Amlogic AXG (`axg_s420_v03`), Cortex-A53, **32-bit armhf** userland, glibc **2.23** (the armhf build's target), kernel 4.9, **236 MB RAM** |
| Storage | `/data` partition (several GB) | `/data` → `/bkupgrade/data` (ubifs, ~81 MB free); rootfs ubifs ro, 87 % full | `/data` → `/bkupgrade/data` (ubifs, ~20 MB free); rootfs ubifs ro, 83 % full | `/data` → `/bkupgrade/data` (ubifs, ~61 MB free); rootfs ubifs ro |
| Capture | `hw:0,2`, 8 ch @ 96 kHz, S16_LE: mics 0–3, speaker ref 4–7 | `hw:0,1`, 8 ch @ 48 kHz: mic 0 on ch 0 (= ch 1), mic 1 on ch 2 (= ch 3), speaker feedback on ch 6 (the VQE speaker reference: it leaves ~25–30 dB less echo in the VQE output than ch 4); ch 4/5 only carry a start-up transient. Max buffer 2048 frames (~42 ms) | `default` (multi in `/etc/asound.conf`), 6 ch @ 48 kHz: PDM mics 0–3 from `hw:0,1` ch 0/2/4/6, TDM amplifier loopback 4–5 from `hw:0,0` | `hw:0,2`, 8 ch @ 48 kHz, S32_LE with right-justified 24-bit samples (`CaptureFormat::S32 { shift: 8 }`): mics 0–3, speaker refs 6/7; ch 4/5 carry a large DC offset and are not fed to the VQE |
| Playback | `speakers` | `speakers` | `default` (softvol `Master` over `dmix_spk_plug` → `hw:0,0`); no `speakers` PCM | `mini` (dmix over `hw:0,0`, see Volume) |
| VQE preset | `cucumber`: 4 mics, 4 spk, chunk 3072 | `plum`: 2 mics, 1 spk, 48 kHz, chunk 1536 → 512 @ 16 kHz; outputs 1, 2, 4, 6, 8, 9 | `yandexmini_2`: 4 mics, 2 spk, 48 kHz | `yandexmini`: 4 mics, 2 spk, 48 kHz, chunk 768 → 256 @ 16 kHz; 32-bit capture |
| Spotters | alisa: types 0, 3, 4; command: type 0 | alisa/yandex: types 0 and 4 (1 ch each); command: type 0. Models in `spotter_models/` are plum-tuned | same tarball as the Lite 2 in `spotter_models/`, not yet measured | same firmware models, not yet measured |
| Ring | `/dev/fb0` 24×1 + `/dev/fb1` logo 6×1, BGR | `/dev/fb0` "YNDX LED ring" 3×1, 24 bpp, BGR; trigger `led_ring_fb`; sysfs `plum:<color>:ring1..3`; no logo | no framebuffer or trigger: sysfs `mini2:<color>:ring1..6` (IS31FL3236 on i2c 1-003c); no logo | six RGB LEDs behind an IS31FL3236-compatible controller at `/dev/i2c-0@0x3c`; no framebuffer, no logo |
| Clock | `cucumber:white:clock-<seg><digit>`, dimming via chip `current`; no centre dot | `plum:white:clock<SEG><pos>`: A–G × 0–3, colon `DH`+`DL`; `H0..3`, `DEGREE` kept dark (`quasar.cfg` `/leds/clock`); `DM` = centre dot at g height (`Model::clock_center`, extra LED 35 of the stock patterns [inference: position not confirmed by eye]); dimming via per-LED `brightness` 0–255 | wired as `mini2:white:clock<A..G><1..4>`, colon `PLUS1`+`PLUS2`, `DEGREE`/`MINUS` dark (`quasar.cfg`), but the second IS31FL3236 (i2c 0-003f) fails to probe on firmware 0.369: the clock finds no segment (`Model::has_clock` false); its entities exist only when `pulsar_clock::panel_present` finds one at start | none (`clock_dir=""`; all clock entities hidden) |
| Keys | `awinic_sar`: 582 action, 115/114 volume | `aw9610x`: same codes | `sar0_pad0` (aw9610x SAR, event0): same codes | `adc_keypad`: only `KEY_MUTE` 113 (`Model::mute_key`): the mic mute button toggles the software mute (persisted at once); no TALK pad, no volume keys, no «Button» event entity (`Model::has_button_events`) |
| Mic mute | input `mute`: `SW_MUTE_DEVICE` | input `gpio-keys`: `SW_MUTE_DEVICE` (+ `KEY_MUTE`) | no evdev switch: GPIOZ_3 (global 414, pull-up), active low, polled every 100 ms as maind does (`Model::mute_gpio`; exported if needed, no `direction` / `edge` write, used only when already an input) | no hardware mute (`mute_input=""`): the mute button toggles the software switch (see Keys) |
| Light sensor | `ltr308als01` (iio) | same | `ltr308als01` on the #38 station (firmware 0.372; `in_intensity_input` = raw / 120, no `/etc/light-sensor.conf`), absent on the #9 station (0.369) [inference: firmware, board revision or device tree; not known which]; `Model::has_lux` false: the Illuminance entity exists only when the sensor opens at start | none |
| Volume | "Master Playback Volume" softvol on `speakers` | same names; `speakers` comes from `/etc/init.d/S45amplifier-settings` (`/tmp/asound-vol.conf`) | same control on `default`; `set_volume.sh` is `amixer set 'Master' <percent>%`, so the curve is linear in percent (`volume_min_step` false) | software: no usable ALSA master; the controller applies the stock dB curve (`pulsar_core::volume::master_gain`) to both player channels and Sendspin; same `STOCK_VOLUME` map and `volume_min_step=true` as the other models; sink is `mini` (dmix over `hw:0,0`, `/data/pulsar/asound.conf`) |
| Output gates | none | none | — | `/sys/class/tdm_state/amp_table` + `aux_amp` = 1 and ALSA `SPK mute` = 1, enabled by `pulsar_alsa::open_playback(model)` before and after the PCM open (Sendspin sink, ANNOUNCE writer; the open can reset them) and by the MUSIC pipeline's `pulsar_alsa::enable_output(model)` before it starts and again when GStreamer reports PLAYING (the alsasink has the device open by then). dmix turns `amp_table` off when the last client closes, and the driver clears it when the PCM closes: idle is gated off (`aux_amp` stays 1), so the re-enable sits before every start |
| alsa-lib | 1.2.10, versioned | 1.2.1.2, unversioned | 1.2.1.2, unversioned | versioned, outside the loader path: `pulsar_alsa::init` dlopens `Model::alsa_lib` (`/system/vendor/quasar/libs/libasound.so.2`, RTLD_GLOBAL) before GStreamer loads, so `libquasar_gstreamer.so` gets that copy by SONAME (its RUNPATH not checked on a Mini); `ALSA_CONFIG_PATH` is set only when `/data/pulsar/asound.conf` exists (else an error is logged) |
| Earcons | `common/spotter_*.mp3`, `common/volume_sound.mp3`, `timer.wav`, `ru-RU/vins_error.wav` | same except `ru-RU/playback_error.mp3` instead of `vins_error.wav` | `common/spotter_start.opus`, `common/spotter_cancel.opus`, `common/volume_sound.opus` [inference: file not confirmed on the station], `common/timer.opus`, `ru-RU/playback_error.opus` (no MP3 decoder in this GStreamer) | `ru-RU/playback_error.mp3` |
| Boot | `S97quasar` → `onboot.sh`; then `S99zpulsar` → `boot.sh` | `S99quasar` → `onboot.sh`; then `S99zpulsar` → `boot.sh`; `q stop` available | same as the Lite 2 | `S99onboot.sh` → stock stack; then `S99zpulsar` → `boot.sh` |
| Zigbee | Telink B91 on `/dev/ttyS2`, reset/boot via `zb_control` (see [Zigbee radio](zigbee.md)) | none | none (no `ttyS2`, no `zb_control`) | none |

Mini 3 (`bergamot`, experimental): hardware facts and what is unverified in [mini3.md](mini3.md).

Lite 2 capture: reading and VQE on one thread overrun the 42 ms device buffer, so capture has its
own thread with a FIFO. A few overruns an hour remain (`capture: Broken pipe, recovering`); they
are expected.

Stale dmix lock (every model; mechanism in the `supervise.sh` header): a process killed while
playing can leave later players silent until the stale IPC objects are cleared or the station
is rebooted. Restarting the daemon without clearing them does not help.
`supervise.sh` clears it before each launch, `tools/station.sh` in `stop`, `test-stop` and
`cleanup`; by hand: `ipcrm -M 0x400 -M 0x401 -S 0x400` (the Mini's `minidmix`: `ipcrm -M 0x8e5
-M 0x8e6 -S 0x8e5`) when `ipcs -m` shows nattch 0 for both.

Lite 2 LEDs: `led_ring_fb` scales each channel on its way to the LEDs; sysfs `brightness` reads
the framebuffer value × ~0.36 (red), ~0.23 (green), ~0.2 (blue) and caps at 63, so ring colours
checked through sysfs only match the Midi's after that correction (`/dev/fb0` holds the written
B,G,R bytes).

Mini 2 notes (firmware 0.369): the clock chip's probe failure (`register write to 0x4F failed
(error -6)`) is the only reason the clock is dark: the DT node is fine, so a firmware or driver
fix brings the panel back (`panel_present` then enables its entities at the next start); the #38
station (firmware 0.372) has both the panel (28 segments) and the light sensor [inference: what
differs from the #9 station (firmware, board revision, device tree) is not known]. The mic
mute button is momentary, but the microphone circuit latches it: GPIOZ_3 reads 0 while the
microphones are cut and holds that level between presses, so the level is the state; GPIOZ_8
(419) only pulses high during a press. The button emits no evdev event (`sar0_pad0` carries
582/115/114 only), and the press does not update `data/is_speaker_muted`, so that file cannot be
used as the state. Verified on hardware with the C++ daemon of #9 before the Rust port: VQE preset
loading and capture, wake word → HA pipeline → TTS, playback through the firmware's 32-bit
GStreamer on `default`, Sendspin with Music Assistant, the LED ring, touch keys, volume, the GPIO
mic mute and `pulsar-install` end to end. Not yet verified: the clock, spotter quality, the Rust
port on the station.

