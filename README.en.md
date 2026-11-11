English | [Русский](README.md)

# pulsar

Yandex Station Midi, Station 2, Lite 2, Mini 2, Mini 3, Mini 3 Pro and the first-generation Mini
as a local voice satellite and smart speaker for
[Home Assistant](https://www.home-assistant.io/) — no Yandex cloud involved.

| Model | Codename | Status | Model data |
|---|---|---|---|
| Station Midi | `cucumber` | verified by the maintainer | maintainer's speaker |
| Station Lite 2 | `plum` | verified by the maintainer | maintainer's speaker |
| Station Lite 2, `prune` board | `prune` | experimental, see [Lite 2 on the prune board](#lite-2-on-the-prune-board) | contributor: [#57](https://github.com/Morvex885/pulsar/issues/57) |
| Station 2 | `station-midi` | experimental, see [Station 2](#station-2) | contributor: [#58](https://github.com/Morvex885/pulsar/pull/58) |
| Station Mini 2 | `station-mini2` | experimental, see [Mini 2](#mini-2) | contributors: [#9](https://github.com/Morvex885/pulsar/pull/9), speaker from [#38](https://github.com/Morvex885/pulsar/issues/38) |
| Station Mini 3 | `bergamot` | experimental, see [profile](https://github.com/Morvex885/pulsar/blob/main/docs/mini3.md) | contributor: [#30](https://github.com/Morvex885/pulsar/pull/30) |
| Station Mini 3 Pro | `pickle` | experimental, see [profile](https://github.com/Morvex885/pulsar/blob/main/docs/mini3pro.md) | contributor: [#52](https://github.com/Morvex885/pulsar/pull/52) |
| Station Mini | `yandexmini` | per contributor data, see [Mini](#mini) | contributor: [#35](https://github.com/Morvex885/pulsar/pull/35) |

Instead of Yandex's stock stack (`maind`), a small native daemon runs on the speaker. It talks to
Home Assistant over the ESPHome Native API and looks like an ordinary ESPHome device in HA.
For [Music Assistant](https://music-assistant.io/) it is additionally a native
[Sendspin](https://www.sendspin-audio.com/) player.

The speaker's hardware is used in full, through the stock facilities of the firmware:

- **"Alisa" / "Yandex"** — Yandex's own wake-word model and its own noise
  suppression/echo cancellation (VQE) across the speaker's microphones (Midi — 4, Station 2 — 4,
  Lite 2 — 2, Mini 2 — 4, Mini 3 — 3, Mini 3 Pro — 3, Mini — 4), subtracting the sound of its own
  speakers.
- **Local voice commands** — «стоп» (stop), «громче» (louder), «тише» (quieter), «продолжи»
  (resume) and so on are executed on the speaker itself, without HA and without "Alisa" (Yandex's
  own command model).
- **The nearest speaker answers** — if several pulsar speakers hear "Alisa", the one that heard it
  loudest answers.
- **Speakers** — HTTP stream playback through the firmware's GStreamer, volume through ALSA.
- **LEDs — panel or ring, clock, buttons, microphone switch, light sensor** — depending on the
  model; the Mini lacks some of them (see [Mini](#mini)), and Station 2 has an 88-LED panel instead
  of a ring (see [Station 2](#station-2)).
- **Zigbee (Midi, experimental)** — the speaker's built-in Zigbee module running pulsar's own
  firmware acts as a full coordinator for Zigbee2MQTT and ZHA (like a ConBee USB stick) or as a
  router (repeater) in an existing network (see [Zigbee](#zigbee-midi)).

No proprietary Yandex library is distributed in the repository: the daemon loads them from the
speaker itself at runtime.

> The project is not affiliated with Yandex. Anything you do to the device is at your own risk.

## What shows up in Home Assistant

Unless stated otherwise, an entity is present on all models. The clock (all `clock_show_*` entities
and actions) is present on Midi, Lite 2 with the `plum` board and Mini 3 Pro, and on Mini 2 and
Mini 3 if the panel is found at startup; the Mini, Lite 2 with the `prune` board and Station 2 have
no clock. The light sensor is on Midi, Lite 2 with the `plum` board, Mini 3, Mini 3 Pro and
Station 2, and on Mini 2 if it is found at startup. "Clock auto brightness" — when a clock is
present and the sensor opened successfully. The hardware microphone switch is on Midi, Station 2,
Lite 2, Mini 2, Mini 3 and Mini 3 Pro. What the Mini has and doesn't have is in the
[Mini](#mini) section.

| Entity | What it does |
|---|---|
| Assist satellite | Voice assistant: "Alisa" / "Yandex" wake words, timers, announcements (`assist_satellite.announce`), conversation follow-up |
| Media player | Media and TTS playback, volume, mute (total silence: music, responses, announcements and speaker sounds); if the network drops, the stream reconnects (3 attempts) from the same position; visible in Music Assistant through the "Home Assistant" provider |
| Switch "Microphones" | Microphones: on — the speaker listens, off — it doesn't (mute from HA or with the button on the case). The entity used to be called "Mute microphones" with inverted polarity. "Turn off" sets the HA mute, "turn on" clears it if the button mute is not active. If the microphones are turned off only from HA, the mute button has to be pressed twice: the first press sets the button mute, the second clears it and clears the HA mute along with it (on the Mini — see [Mini](#mini)) |
| Switch "Clock display" | The clock on the front panel |
| Switch "Clock auto brightness" | Automatic clock brightness from the light sensor: in the dark the clock dims almost to nothing, in the light it burns at full strength. When off, the clock glows at the "Clock brightness" level under any light |
| Switch "Clock spoken numbers" | Numbers from the voice assistant's reply on the clock; off by default. How they are shown — below, after the timers |
| Number "Clock brightness" | Clock brightness (with auto brightness on — the upper limit, it drops according to the sensor in the dark; with it off — the level as is) |
| Text "Clock text" | Custom text on the clock (up to 64 characters): Latin letters, digits, approximate Cyrillic; `:` lights the colon. Up to 4 characters — centred, longer — scrolling around in a loop until the text is cleared |
| Select "Clock face" | Faces on the clock (Smiley, Sad, OwO, >w< and others) and the Blink / All faces animations; text and face replace each other |
| Number "Clock text speed" | Scrolling speed, ms per step (100–2000) |
| Select "Clock music" | Music on the clock during playback: Off (default) / Levels — each digit has its own level (bottom, middle, top segment) from two adjacent spectrum bands, within how those bands changed over the last seconds (so you see changes rather than the overall timbre of the song); the time is shown periodically. A set "Clock text" or "Clock face" hides "Clock music". Same source as "Music visualization" |
| Select "Clock format" | Time format: 24h (default) / 12h — hours 1–12 without a leading zero, midnight and noon are `12:00`. There is no AM/PM indicator on any model: there is no LED that would suit it on all panels |
| Actions `clock_show_text` / `clock_show_face` | `esphome.<device>_clock_show_text` (`text`, `seconds`) and `…_clock_show_face` (`face`, `seconds`): temporarily show text or a face on top of "Clock text" / "Clock face"; `seconds` ≤ 0 — one scroll pass or 5 s, maximum 3600; an empty value removes the display |
| Light "Ring" / "Panel" | The ring as an ordinary lamp: colour, brightness, smooth transitions (transition, up to 60 s), effects Rainbow, Breathe, Color loop, Comet, Twinkle, Fire, Aurora, Lava lamp, Candle, Night light — identical on all models with a ring (Midi has a strip of 24 LEDs, Mini 3 Pro — 16, Lite 2 — 3, Mini 2, Mini 3 and Mini — 6); if there is a light sensor, the ring and the logo are dimmed in the dark. Station 2 has an LED panel instead of a ring: the entity is called "Panel" and has its own effect list — Radar, Ripple, Rain, Matrix, Plasma, Waves, Spiral, Heartbeat |
| Select "Music visualization" | Light music during playback: Off / All styles (default: Spectrum → Pulse → Disco → Polar in a loop) / Spectrum / Pulse / Disco / Polar. The source is the Sendspin visualizer (Music Assistant) if the server sends it, otherwise the spectrum of the sound the speaker itself is playing. The "Ring" or "Panel" light being on takes precedence over music visualization |
| Number "LED speed" | Speed of effects and music visualization, 0.25–4× |
| Number "Follow-up duration" | How long to wait for the next utterance to start if HA keeps the conversation going after the response: 0–10 s, default 10; 0 — don't listen for a follow-up |
| Number "Command start offset" | A command spoken right after the wake word, without a pause («Алиса, который час» — "Alisa, what time is it"), doesn't lose its beginning: the audio right after the phrase goes into the request as well, starting from the point where the speaker recognised it, shifted by this many ms: −100…200, default 0. If a tail of the phrase shows up in the request text («Так, включи свет» — "so, turn on the light"), shift right by 10–20; if the start of the command is lost («Закрой» instead of «Открой» — "close" instead of "open") — to the left. Shifting right by more than the speaker spends confirming the phrase (usually 150–250 ms) changes nothing: the request starts from the moment of confirmation. Persisted across restarts. Replaces the former "Command right after wake word" switch; if it is left unavailable in HA after the update, it can be deleted |
| Number "Microphone gain" | Gain applied to the voice before it is sent to Home Assistant: 0–40 dB, default 0 — no gain (recognition gets the same channel the stock firmware uses). Raise it if your recognizer handles quiet speech poorly; too much gain overloads the sound and spoils recognition. It does not affect wake-word detection. Persisted across restarts |
| Switch "Voice indication" | Show "listening / thinking / answering" on the ring (panel) and the clock (errors and timer ringing are always visible) |
| Select "Case color" | Colour correction for the case fabric (stock firmware matrices from `/system/vendor/quasar/ledconfigs`); None — no correction; if there are no matrices (the Mini and Station 2 have none, on some Mini 2 firmware the directory is empty), the entity is not published — on any model |
| Action `led_show` | `esphome.<device>_led_show` (`effect`, `color`, `seconds`): temporarily show an effect (or a solid colour when `effect` is empty) in the colour `#RRGGBB` (empty — white) at full brightness on top of the lamp and music visualization; the speaker's temporary signals (volume, assistant, timer, the short microphone-switch signal) go on top of the display; `seconds` ≤ 0 — 5 s, maximum 3600; empty `effect` and `color` remove the display |
| Binary sensor "Hardware mic mute" | Hardware microphone switch on the case (not on the Mini) |
| Sensor "Illuminance" | Light sensor, lx |
| Event "Button" | For automations: `volume_up`, `volume_down`, `action`, `action_long` (not on the Mini) |
| Button "Restart" | Restart the daemon on the speaker |
| Update "Firmware" | A new pulsar version from GitHub Releases: "Install" in HA (see [Updating from Home Assistant](#updating-from-home-assistant)) |
| Switch "Auto update" | Install new versions automatically (off by default) |
| Switch "Nearest station answers" | The nearest speaker answers (on by default, see [Multiple speakers](#multiple-speakers)) |
| Diagnostics | "Wi-Fi signal" (dBm), "Uptime", "IP address", "Version" |

The time zone for the clock comes from Home Assistant. The daemon log is visible in HA (ESPHome
device logs). Several clients can connect to the speaker at once (up to 4), for example HA and
ESPHome Dashboard; the voice pipeline serves the last client subscribed to it, and when that one
disconnects or unsubscribes — the previous one again (usually HA).

"Follow-up duration" is persisted across restarts. The countdown starts from the moment HA begins
listening for the follow-up; if you start speaking, the wait is cancelled and the utterance is not
cut off. If there is no response, the conversation ends quietly, with no error on the ring. The time
limit works when HA itself detects the start of speech; with a recognizer that detects the end of
the phrase itself (for example HA Cloud STT v2 from Labs), the setting only enables or disables the
follow-up. 0 also closes an already open wait.

Timers: the clock shows the countdown of the nearest of the running timers (if all are paused — the
timer with the least remaining time); it is shown instead of `clock_show_*`, "Clock text" and "Clock
face", and short animations (pause, track change) briefly cover the countdown.
If HA doesn't advance the dialogue (30 s of listening or 120 s of processing without events), the
speaker ends it itself and shows an error on the ring.

The priority on the clock: pairing code > volume when the buttons are pressed > timer ringing > a
number from the reply > the voice assistant > update installation (the clock goes dark) > startup
and pause / track change animations > timer > `clock_show_*` > "Clock text" / "Clock face" >
"Clock music" > time.

Numbers from the assistant's reply are shown only if "Clock spoken numbers" is on (off by default).
Two-digit numbers alternate sides: the first on the left, the second on the right, the third on the
left. Each one stays for at least 3 s; a long number has time to scroll through in full. The numbers
are shown in order after playback of the reply starts; late text starts the display from the first
number. There is no exact synchronisation with the words. After the reply ends, the remaining
numbers keep being shown, including when the follow-up ends without a new utterance; a cancellation
and a new dialogue remove them, a new reply replaces them. If there is no text, there are no
numbers. Digits, Russian numerals up to 99 and time (`HH:MM`, in the "Clock format") are supported.
The source of the text is the
[HA voice assistant](docs/voice.md#голосовой-ассистент-ha-esphome).

Text and face on the clock are not persisted across restarts (after a restart — the time), while
speed, "Clock music", "Clock format" and "Clock spoken numbers" are. The "Clock display" switch turns
off everything except the Sendspin pairing code.

### Buttons and indicators

- **Action button:** short press — start a dialogue without a wake word; while the speaker is
  listening or waiting for a response — cancel the dialogue; during a response — interrupt it; when
  a timer rings — turn it off. A long press only sends an event to HA.
- **Volume +/−:** 5 % steps (from an intermediate volume — up to the nearest step), the level is
  shown on the ring (the panel) and on the clock (if present), with the stock click; the same click
  for «громче» (louder) / «тише» (quieter). No click when the level didn't change (0 % or 100 %),
  while the speaker is listening or answering, during an announcement and a timer ring, and when the
  volume is changed from HA or Music Assistant. On Midi, Lite 2 with the `plum` board, Mini 2,
  Mini 3 and Station 2 the volume scale is the same as the stock firmware's: 5 % is one step of it,
  10 % in HA corresponds to "1" on Alisa (on the Mini — see [Mini](#mini)).
- **Ring (on Station 2 — the panel):** animations in the stock Alisa style (same colours and
  rhythm, drawn anew): listening, thinking, answering, error, timer ringing, timer started (green
  dots fade over 15 s), volume. Microphones off: on Midi and Mini 3 Pro — a red wave along the strip
  and a red logo, on Lite 2, Mini 2 and Mini 3 — a flash and an even dim red, on the Mini — a red
  ring, on Station 2 — a red wave across the panel and an even dim red; on all models except Midi
  and Mini 3 Pro this red overrides the light ("Ring" or "Panel") and music visualization, but
  yields to the speaker's temporary signals (volume, assistant, timer, `led_show`). A red flash
  after "Alisa" means the HA voice assistant is not subscribed to the speaker (HA is not connected
  or no assistant is selected).
- **Clock (Midi, Lite 2 with the `plum` board, Mini 3 Pro; on Mini 2 and Mini 3 — if the panel is
  found at startup):** stock Lite 2 firmware animations (drawn anew, identical on these models):
  "eyes" at daemon startup, listening / thinking / answering (with "Voice indication"), timer
  ringing, "||" on pause (by voice or from HA) and running bars on «дальше» (next) / «назад»
  (previous). The middle dot between the colon dots exists only on Lite 2 with the `plum` board,
  Mini 3 and Mini 3 Pro.

### Local voice commands

The command model listens always, as on stock: in silence, on pause, during music, an assistant
response, an announcement and a timer ring. While you are talking to HA and while it is thinking,
all commands work only for the first 2.5 s after "Alisa" («Алиса, громче» — "Alisa, louder"), so
that the words of the request don't trigger as commands. Later only «стоп» (stop) and «хватит»
(enough) work: they cancel the dialogue like a short button press (no response, music keeps
playing; if a timer is ringing — they turn the ring off). If HA has already recognized the request
and started executing it, what was done is not rolled back. These words inside the request itself
(«хватит ли бензина» — "is there enough petrol") will cancel it too. Without "Alisa" the speakers
don't negotiate who executes a command, so each executes only its own: «стоп» and «хватит» —
everywhere; «громче» and «тише» — if the speaker is sounding (music, response, announcement, timer)
or its music is paused; «пауза», «продолжи», «дальше», «назад» — only for its own music. Paused
music (or stopped in Music Assistant) stays its own for 2 hours.

During a dialogue pulsar also checks HA's finished recognition text, so «стоп» works even if the
command model missed it. The check accepts only stop words without other words of the request,
including repetition («стоп, стоп»). It cancels the dialogue and turns off the timer ring, and if
this very request was started with the word "Alisa" (rather than by the button, a follow-up or from
HA), it acts as «Алиса, стоп» from the table below.

The command model recognises Russian phrases only: the English in brackets is their translation.

| Phrase | Action |
|---|---|
| «стоп» (stop) / «хватит» (enough) | turn off the timer ring, otherwise cancel the dialogue or interrupt the response, otherwise pause the music («Алиса, стоп» — "Alisa, stop" — cancels the request and pauses the music) |
| «пауза» (pause) | pause |
| «продолжи» (resume) / «играй» (play) | resume playback |
| «громче» (louder) / «тише» (quieter) | volume one Alisa scale step (10 %) up / down; from an intermediate volume — to the nearest step |
| «дальше» (next) / «следующий» (next) / «назад» (previous) | next / previous track (Sendspin) |

### Multiple speakers

Pulsar speakers on the same network agree on who answers: on hearing the wake word, each tells its
neighbours on the network how loudly it heard it, and waits briefly for their replies. The speaker
that heard the phrase loudest answers, that is usually the nearest one; a speaker that is answering
at that moment, reading an announcement or ringing a timer does not give up its turn. On the others
the ring blinks and goes out, there is no sound, and «Алиса, громче» (Alisa, louder) changes the
volume only on the one that answered. The activation sound lags by fractions of a second because of
the wait; what is said right after "Alisa" is not lost.

With "Nearest station answers" off (or if packets don't get through: the access point cuts broadcast
traffic, speakers are on different subnets), the one whose request reached HA first answers: HA
rejects the rest as duplicates, and they go dark silently. Other voice satellites (Voice PE and the
like) don't take part in the agreement.

## Requirements

**Speaker:**

- Yandex Station Midi, Station 2, Lite 2, Mini 2, Mini 3, Mini 3 Pro or the first-generation Mini
  with root access over adb: `adb shell` as root, over the network or USB. The root itself is not
  part of this project.
- Firmware: on Midi (`0.351.1.6.4197281754.20260528.142`) and Lite 2 with the `plum` board
  (`0.369.1.46.4778258659.20261013.133`) it has been verified by the maintainer. Contributor data:
  Mini 2 — firmware `0.369.1.46.4778262943.20261013.299`, Mini 3 — `0.369.1.50.4794652521.20261021.117`,
  Mini (`axg_s420_v03` board) — `1.369.4.24.4711907614.20260928`, Lite 2 with the `prune` board —
  `0.369.1.50.4794654278.20261021.133`, Station 2 — `0.369.1.46.4793639783.20261020.295`; Mini 3
  Pro — firmware not specified.

**Computer:** Windows, macOS or Linux on the same network as the speaker. Nothing needs to be
installed: the installer downloads `adb` itself if it is missing (see [Installation](#installation)).

**Home Assistant** 2025.x or newer with the ESPHome integration and a configured Assist pipeline
(STT/TTS with Russian support).

## Installation

Download the archive for your computer from the
[latest release](https://github.com/Morvex885/pulsar/releases/latest), unpack it and run
`pulsar-install`.

| Computer | Archive |
|---|---|
| Windows | `pulsar-windows-amd64.zip` |
| macOS (Apple Silicon) | `pulsar-macos-arm64.tar.gz` |
| macOS (Intel) | `pulsar-macos-amd64.tar.gz` |
| Linux x86-64 | `pulsar-linux-amd64.tar.gz` |
| Linux ARM64 (e.g. Raspberry Pi) | `pulsar-linux-arm64.tar.gz` |

Linux and macOS (substitute the name of your archive):

```sh
curl -fLO https://github.com/Morvex885/pulsar/releases/latest/download/pulsar-linux-amd64.tar.gz
tar -xzf pulsar-linux-amd64.tar.gz && cd pulsar
./pulsar-install                 # find speakers on the network and pick where to install
./pulsar-install 192.168.1.50    # or straight by address
./pulsar-install --list          # only show what was found
```

Windows (PowerShell or command prompt):

```bat
curl.exe -fLO https://github.com/Morvex885/pulsar/releases/latest/download/pulsar-windows-amd64.zip
tar -xf pulsar-windows-amd64.zip
cd pulsar
.\pulsar-install.exe
```

You can also unpack the archive in Explorer and launch `pulsar-install.exe` by double-clicking (see
[Windows and macOS](#windows-and-macos)).

The archive already contains programs for all supported speaker models and dropbear, with the
helper scripts alongside.

The installer needs `adb`. If it is not in `PATH`, on first run the installer downloads Android SDK
Platform-Tools from dl.google.com (Google's terms) into the `platform-tools` directory next to
itself. For Linux ARM64 Google doesn't build them — install `adb` yourself (`sudo apt install adb`).

Without an address the installer finds Stations over mDNS (both stock ones and ones already switched
to pulsar). Then it connects to them over adb, determines the model and shows a list. Supported
speakers with root access are numbered. The rest (other models, adb without root) are listed with
the reason for skipping.

If the speaker is not found, `--scan` looks for open adb (port 5555) across the whole /24 subnet.
Other Android devices on the network may then show an "Allow debugging?" prompt — you can decline
it.

The installer from a repository checkout builds the program first: build tools are required (see
[docs/development.md, "Build"](https://github.com/Morvex885/pulsar/blob/main/docs/development.md#сборка)).
Then, for each selected speaker, it:

1. copies the daemon and the helper scripts to `/data/pulsar`; if the binary changed, the previous
   one is kept as `.prev` (the same binary is not copied and `.prev` is left alone);
2. writes the autostart script `/etc/init.d/S99zpulsar`. At boot the system runs it after Yandex's
   script; it starts `/data/pulsar/boot.sh` and dropbear, if the latter is installed. To write it,
   the root filesystem is remounted read-write for a few seconds and immediately returned to
   read-only; if the file is already up to date, nothing is remounted. If the root filesystem is
   protected (dm-verity, device-mapper) or `/etc` is not on it, the script is not written: the
   installer reports the reason and says at the end that pulsar won't start by itself after a
   reboot;
3. if the previous version added autostart lines to `/data/quasar/rootkeeper.sh`, removes exactly
   those lines (backup `rootkeeper.sh.bak-<time>`); the rest of the file's contents are not changed;
4. stops the Yandex stack and starts the daemon; after ~10 s and once more after ~30 s it checks the
   log that the daemon came up and didn't crash, and on failure offers to put the previous binary
   back.

Outside `/data` the installer writes only `/etc/init.d/S99zpulsar`, the system partition is not
touched otherwise. Re-running updates the installed version. `--yes` installs on all found speakers
without questions, `--no-enable` doesn't stop the Yandex stack and doesn't start the daemon (step 4).

Next, in Home Assistant: **Settings → Devices & services**. The speaker is discovered by itself as
an ESPHome device `yandex-midi-xxxxxx`, `yandex-station2-xxxxxx`, `yandex-lite2-xxxxxx`,
`yandex-mini2-xxxxxx`, `yandex-mini3-xxxxxx`, `yandex-mini3pro-xxxxxx` or `yandex-mini-xxxxxx`. If it
wasn't discovered, add the ESPHome integration manually with the speaker's IP and port `6053`; HA
creates the encryption key itself (see
[API encryption](#api-encryption)). In the device settings choose the Assist pipeline and the wake
word.

### SSH (optional)

```sh
./pulsar-install --ssh            # also installs dropbear
```

Installs dropbear into `/data/dropbear` on port 22. **Root login with an empty password** (`-B`).
Install only on a trusted network or change the flags in `ssh/start.sh`.

### Windows and macOS

> The installer for Windows and macOS has not been tested on real computers yet. If something
> doesn't work, write to the issues.

- **Windows.** `pulsar-install.exe` is a console program: if you launch it by double-clicking, the
  window waits for Enter at the end so the output can be read. An archive downloaded by the browser
  and unpacked in Explorer may trigger SmartScreen: "Windows protected your PC" → "More info" →
  "Run anyway"; an archive downloaded with `curl.exe` and unpacked with `tar`, as in the
  [Installation](#installation) section, has no such mark. If `adb` is already in `PATH` (for
  example from Android Studio), it is used. Connecting the speaker over USB is not described: use
  adb over the network.
- **macOS.** Download the archive with `curl`, as in the [Installation](#installation) section. For
  an archive already downloaded by the browser: `xattr -dr com.apple.quarantine pulsar`. If the
  search finds nothing or adb says "No route to host", allow the terminal access to the local
  network: "System Settings" → "Privacy & Security" → "Local Network". Building the daemon on macOS
  is not supported — use the archive.

## Updates and rollback

### Updating from Home Assistant

The speaker checks [releases](https://github.com/Morvex885/pulsar/releases) a minute after startup
and then every 6 hours; to check right away — "Check for updates" in HA
(`homeassistant.update_entity`). Only stable releases are considered (not pre-releases). When a new
version comes out, the "Firmware" entity in HA shows it with a link to the release; the "Install"
button installs it. The speaker downloads `pulsar-<arch>.bin` over HTTPS (the certificate is
verified), compares it against the release's `SHA256SUMS` and checks that the program runs on this
speaker and reports the right version. Only after that does it replace the binary (the previous one
stays as `.prev`) and restart — a conversation or music will be interrupted at that moment. While
the installation runs, the clock is dark; if the installation failed, it shows the time again.

With the "Auto update" switch on, the speaker installs a new version itself, after waiting for
silence (no dialogue, music, announcement or timer ring). If the new version doesn't start,
automatic rollback (see below) brings the previous one back, and auto-update will no longer install
that version; it can be installed manually.

Builds not from a release (the version is a commit hash, e.g. `1fa4934`) are seen by the updater
too, but are not updated automatically: only with the button. The daemon is updated (on Midi — the
Zigbee module images too); `boot.sh`, `supervise.sh` and `ctl.sh` are updated by the installer.

### Automatic and manual rollback

If the daemon crashes 5 times in a row without having run for 5 minutes, `.prev` is installed
instead (the broken binary is kept as `.bad`); if that one crashes too, the speaker returns to the
Yandex stack, as after `ctl.sh disable`. A restart with the "Restart" button in HA or via
`ctl.sh restart` doesn't count as a crash if the daemon had run for at least 5 s before that.

To roll back to the previous version manually:

```sh
cd /data/pulsar && mv pulsar.prev pulsar && ./ctl.sh restart
```

### Uninstalling

Return to the Yandex stack, remove `/data/pulsar` and the autostart script (the root filesystem has
to be remounted read-write briefly):

```sh
/data/pulsar/ctl.sh disable
rm -rf /data/pulsar
mount -o remount,rw / && rm -f /etc/init.d/S99zpulsar; mount -o remount,ro /
```

## Music Assistant

Music Assistant with Sendspin support finds the speaker by itself: the player "Yandex Station Midi",
"Yandex Station 2", "Yandex Station Lite 2", "Yandex Station Mini 2", "Yandex Station Mini 3",
"Yandex Station Mini 3 Pro" or "Yandex Station Mini" with type **Sendspin**. Music is lossless
(FLAC), the speaker's volume and voice commands control the MA player. Synchronized playback in
groups with other Sendspin players is provided for by the protocol, but has not been tested yet. The
output delay is set in the Sendspin player settings in Music Assistant; MA stores it itself and
sends it on every connection. For Midi Yandex's firmware
uses a correction of about 22 ms (not measured) — you can start from that.

By default the speaker accepts Sendspin connections without pairing. To require pairing:

```sh
adb shell 'echo 0 > /data/pulsar/sendspin/unpaired_access'
adb shell /data/pulsar/ctl.sh restart
```

After that, "----" appears on the clock when pairing is requested: press the action button and the
speaker shows the pairing code. The Mini has no clock and no action button: the pairing code is
written to the log (`/data/pulsar/ctl.sh log`).

## Zigbee (Midi)

> Experimental. Tested on a single Midi: coordinator in Zigbee2MQTT (zigbee-herdsman) and ZHA
> (zigpy) — creating a network with the parameters from their config, restarts without
> re-creation, air scanning, connecting an Aqara button (interview, presses, module firmware update
> without losing the network); router in a Zigbee2MQTT network. Migrating a live network from a
> stick and large networks have not been tested yet.

Midi has a Zigbee module that runs the whole Zigbee stack; the network (PAN ID, keys, joined
devices) is stored in its own flash memory. pulsar installs its own firmware into the module (built
from `radio/` on the open Telink Zigbee SDK and installed alongside the daemon). The role is
selected in HA (the "Zigbee role" entity):

- **Off** (default) — pulsar leaves the module alone.
- **Coordinator** — network coordinator for Zigbee2MQTT and ZHA, like a ConBee USB stick: the
  speaker is specified in their settings, and all further management (pairing, devices, groups,
  backups) is done from their interface. Network parameters (PAN ID, extended PAN ID, key, channel,
  coordinator IEEE address) are set by Zigbee2MQTT or ZHA.
- **Router** — the speaker becomes a router (repeater) in someone else's network, like any
  mains-powered Zigbee device: for example, in a Zigbee2MQTT network with a USB stick. Port 6638 is
  closed in this role.

When a role is selected, pulsar restarts and, if the module doesn't have the right firmware, writes
the needed one through the module's bootloader (20–30 seconds; progress is visible in the "Zigbee"
sensor; restarts and updates from HA are not performed during this time). Each firmware stores its
own network in its own area of flash memory, so the networks of both roles survive switching back
and forth; the network Yandex created also stays in the module, and Yandex's firmware will come back
by itself if the native stack is enabled (`ctl.sh disable`). If a pulsar update brings a new build
of the current role's firmware, it is written to the module by itself on the first start (the
network is preserved). If writing was interrupted (for example the power went out), pulsar will
write the firmware again on the next start. Firmware the module did not accept is not written again
until the role changes or the next update.

### Coordinator

pulsar exposes the module over the deCONZ (ConBee/RaspBee) protocol on TCP port **6638**:

- **Zigbee2MQTT**:
  ```yaml
  serial:
    port: tcp://<speaker IP>:6638
    adapter: deconz
  ```
  On first start Zigbee2MQTT creates a network in the module with its own `pan_id`, `ext_pan_id`,
  `network_key` and channel; after that, restarts don't touch the network. To migrate a network
  from another adapter (for example a USB stick), keep the same `pan_id`, `ext_pan_id`,
  `network_key` and channel in the config and the `coordinator_backup.json` file: Zigbee2MQTT will
  write the IEEE address and the frame counter from the backup into the module, and the devices
  should stay on the network (not tested on a live network). Individual Zigbee 3.0 device keys
  issued by the old adapter are not migrated: such a device keeps working, but if it loses the
  network (channel change, long disconnection), it will have to be paired again. Disable the old
  adapter after the migration — two coordinators on one network interfere with each other.
- **ZHA** finds it by itself (mDNS `_zigbee-coordinator._tcp`): "Settings → Devices & services" will
  show the discovered Zigbee coordinator "Yandex Station Midi". Manually: ZHA → radio type
  **deCONZ**, path `socket://<speaker IP>:6638`.

Only one client works at a time: a new connection evicts the old one. Port 6638 is open without
authentication (as with network coordinators in general): anyone on the local network can connect,
evict HA, open the network for joining, control devices or erase the network.

Limitations: device keys only via the common ZigBeeAlliance09 key (install code is not supported),
the coordinator's own neighbour and route tables are not exposed (the network map is built from
routers), Green Power (battery-less buttons) is not supported.

### Router

Connecting to a network in the Router role:

1. In Zigbee2MQTT (or ZHA) allow devices to join ("Permit join").
2. In HA press the "Zigbee join network" button on the speaker. The module looks for an open network
   on all channels; after a few seconds the speaker appears in Zigbee2MQTT as a router, and the
   "Zigbee" sensor shows "router: in network …".
3. The "Zigbee leave network" button takes the speaker off the network (the coordinator receives a
   Leave).

In the router role the speaker connects to the first open network it finds: keep joining open only
on your own network.

## Control

On the speaker (`adb shell` or ssh):

```sh
/data/pulsar/ctl.sh status    # what is running now: the daemon or the Yandex stack
/data/pulsar/ctl.sh log 100   # log (/tmp/pulsar.log)
/data/pulsar/ctl.sh restart
/data/pulsar/ctl.sh reload    # restart the supervisor script too (after updating supervise.sh)
/data/pulsar/ctl.sh api-key   # whether the API encryption key is set (the key itself is not printed); see below
/data/pulsar/ctl.sh disable   # return to the Yandex stack, including after a reboot
/data/pulsar/ctl.sh enable
/data/pulsar/pulsar --version
```

The log lives in `/tmp` (reset on reboot) and rotates at 1 MB.

Settings (volume, light, clock, wake word, time zone, auto-update) are stored in
`/data/pulsar/state.conf`; after a rollback to `.prev` the speaker reads the same file and plays just
as loudly.

### API encryption

The speaker supports ESPHome API encryption (Noise), like ordinary ESPHome devices. While there is
no key, the API works without encryption, and a fresh Home Assistant creates a key itself on
connection and writes it to the speaker (`/data/pulsar/api_key`); after that the connection is
encrypted. To set the key manually (base64 of 32 bytes, as in ESPHome) or remove it:

```sh
/data/pulsar/ctl.sh api-key <base64>   # set the key, the daemon restarts; then enter it in HA
/data/pulsar/ctl.sh api-key clear      # delete the key, the daemon restarts
```

After changing or deleting the key manually, HA will ask you to re-authorize the device. **Note:**
daemon versions without encryption support (and a rollback to one via `.prev`) will not be able to
talk to an HA that already stores a key: delete the key in the device settings in HA or delete and
re-add the device.

## Models: specifics

### Lite 2 on the prune board

Contributor data ([#57](https://github.com/Morvex885/pulsar/issues/57)); pulsar has not yet run on
this board. The firmware (`deviceType` `plum`), the audio path and the ring are the same as on
Lite 2 with the `plum` board; there is no clock and no light sensor, so there are no clock or
"Illuminance" entities either. The installer identifies the speaker as `Yandex Station Lite 2
(prune)`, in HA it is called `yandex-lite2-xxxxxx`. What is tested and what isn't is in
[docs/lite2.md](https://github.com/Morvex885/pulsar/blob/main/docs/lite2.md).

### Station 2

Contributor data ([#58](https://github.com/Morvex885/pulsar/pull/58)); what is tested and what isn't
is in [docs/station2.md](https://github.com/Morvex885/pulsar/blob/main/docs/station2.md). The
installer installs the armhf build: the kernel on the speaker is 64-bit, user space is 32-bit.

- Instead of a ring there is an 88-LED panel: the lamp is called "Panel" and its effects are its own
  (see [What shows up in Home Assistant](#what-shows-up-in-home-assistant)).
- No clock; there is a light sensor, a hardware microphone switch, an action button and volume
  buttons.
- The codename in the device tree is `station-midi`: this is Station 2's internal name, not the
  retail "Station Midi" (`cucumber`).
- Zigbee is not supported: the radio of this model is built differently from the Midi's.

### Mini 2

The installer installs the armhf build: the kernel on the speaker is 64-bit, user space is 32-bit.

- The voice scenario with HA, playback (including Sendspin), the 6-LED ring, buttons and microphone
  muting were tested on an early daemon version
  ([#9](https://github.com/Morvex885/pulsar/pull/9)). There is no logo and no Zigbee.
- Recognition quality has not been measured: Mini 2 has no spotter models of its own, Lite 2 models
  are used.
- The clock and the light sensor are not on all speakers: the speaker from #9 (firmware 0.369)
  doesn't have them — probably this is the variant without a clock: according to the
  [wiki](https://hackalice.me/devices/station-mini2/) the Mini 2 is sold with and without a clock;
  the speaker from [#38](https://github.com/Morvex885/pulsar/issues/38) (0.372) has them.
  Brightness adjustment by sensor on Mini 2 has not been tested yet.

### Mini 3

An experimental profile: what is tested and what isn't is in
[docs/mini3.md](https://github.com/Morvex885/pulsar/blob/main/docs/mini3.md). There is a light
sensor; the clock — if the panel is found at startup.

### Mini 3 Pro

An experimental profile from contributor data
([#52](https://github.com/Morvex885/pulsar/pull/52)): what is tested and what isn't is in
[docs/mini3pro.md](https://github.com/Morvex885/pulsar/blob/main/docs/mini3pro.md). The installer
installs the aarch64 build. There is a 16-LED ring, a logo, a clock and a light sensor; the speaker
has a Zigbee module, but pulsar does not use it. Autostart after a reboot does not survive on all
firmware — see
[docs/mini3pro.md, "Userland and storage"](https://github.com/Morvex885/pulsar/blob/main/docs/mini3pro.md#userland-и-хранилище).

### Mini

Contributor data ([#35](https://github.com/Morvex885/pulsar/pull/35)). The installer installs the
armhf build.

- No clock, light sensor, hardware MUTE switch, TALK button, volume buttons, logo or Zigbee; so
  there are no clock, "Illuminance" or "Button" event entities either. The ring is 6 LEDs.
- The microphone-off button toggles a software mute: the state is saved to disk immediately and
  survives a restart. A dialogue can be started from the speaker itself only with the
  "Alisa" / "Yandex" phrase.
- Volume is software (via HA, by voice or Sendspin), along the same dB curve as softvol on the other
  models: one 5 % step is the same ~2.5 dB, so 6 % in HA sounds as quiet as on the Midi.
- The Sendspin pairing code is printed only to the log. A USB player and an AirPlay receiver are not
  supported.

## Limitations and risks

- Until HA writes the encryption key, the API is open without encryption to anyone on the network:
  add the speaker to HA right after installation (or set the key via `ctl.sh api-key`).
- The speaker still uses the firmware's DNS (`unbound`) and NTP, which are configured for Yandex
  servers, and remains tied to the Yandex account.
- pulsar does not block or track Yandex firmware updates: protection against OTA is the job of the
  speaker's owner and of the root method. A firmware update can replace the system partition, and
  then pulsar's autostart (`/etc/init.d/S99zpulsar`) and the changes made by the root method will be
  gone. Autostart comes back by re-running the installer, if root access survived.
- Zigbee (experimental): port 6638 without authentication, install code and Green Power are not
  supported, pulsar does not use Yandex's network with Alisa devices; as a router the speaker joins
  any open network — see [Zigbee](#zigbee-midi). Lite 2, Mini 2, Mini 3 and Mini have no Zigbee;
  Mini 3 Pro has the module, but pulsar does not use it; on Station 2 Zigbee is not supported.
- SSH — root login without a password, only on a trusted network (see [SSH](#ssh-optional)).
- The installer for Windows and macOS has not been tested on real computers.
- Mini, Mini 2, Mini 3, Mini 3 Pro, Lite 2 with the `prune` board and Station 2 are contributor
  data; Mini's limitations are in the [Mini](#mini) section.
- The voice of the responses is the one selected in Home Assistant TTS; there is no Alisa voice.

## For developers

Building from source is Linux or WSL2 only; tools and their versions —
[docs/development.md, "Build"](https://github.com/Morvex885/pulsar/blob/main/docs/development.md#сборка).
The daemon's design, station hardware, Zigbee and the on-speaker test protocol —
[docs/](https://github.com/Morvex885/pulsar/tree/main/docs); repository rules and procedures for
developers and agents —
[AGENTS.md](https://github.com/Morvex885/pulsar/blob/main/AGENTS.md) and
[.agents/skills/](https://github.com/Morvex885/pulsar/tree/main/.agents/skills).

## License

[GNU AGPL-3.0](LICENSE). The Zigbee radio firmware (`radio/`) is based on the Telink Zigbee SDK and,
like it, is distributed under the Apache License 2.0. Third-party libraries downloaded at build time
are distributed under their own licenses: sendspin-cpp, ArduinoJson, noise-c, IXWebSocket,
micro-flac, micro-opus, dropbear, OpenSSH. The installer is written in Go (standard library and
`golang.org/x/net`, BSD license). `adb` is not included in the archives: the installer downloads
Android SDK Platform-Tools from Google's site, they are distributed under Google's terms.
