#!/usr/bin/env python3
"""ESPHome API check harness for pulsar (aioesphomeapi, see requirements.txt).

    esphome_check.py --host H --port P [--psk BASE64] <command> [options]

Commands:
  dump        DeviceInfo, entities (in the order the device lists them), user services and the
              initial states, one stable text line each, for diff. --mask replaces what differs
              between builds and hosts: version, compilation time and MAC; the state of uptime,
              version and illuminance (published every 2 s by api_selftest); state and presence
              (missing_state) of the Wi-Fi signal and the IP address.
  commands    one command per entity type, waiting for the echo state where the device sends one.
  voice-fake  subscribes to the voice assistant and plays HA's side of a pipeline run: on start
              RUN_START, STT_END, INTENT_END, TTS_END (--tts-url), RUN_END; prints the audio.
  announce    --media-url URL [--text T] [--start-conversation]: waits for AnnounceFinished.
  timer       --event started|updated|cancelled|finished --seconds N [--id ID] [--name NAME].
  set-key     --key BASE64 (empty: clear): NoiseEncryptionSetKeyRequest.

Scenarios against the daemon (a test instance: PULSAR_PORT_OFFSET, service stopped):
  scenario-voice     announcement (--media-url) with start_conversation; the daemon then starts a
                     pipeline, answered by a fake run (RUN_START, --listen-s, STT_END,
                     INTENT_START, --think-s, INTENT_END, TTS_END with --tts-url, RUN_END); waits
                     --speak-s for the answer to play out and checks that no other run starts
                     (continue_conversation=0) and the media player ends IDLE.
  scenario-timer     timer STARTED (--seconds, default 70 here), --hold-s, then FINISHED and
                     --ring-s of ringing. Nothing in the API stops the ring (button, wake word or
                     "стоп" do; it ends by itself after 15 min): stop the instance afterwards.
  scenario-media     play --media-url, pause, resume, volume 0.1, mute, unmute, volume back (at
                     most 0.2), stop; each checked on the media player state.
  scenario-entities  every switch, number, select, the text, the light and the actions, each
                     checked on its echo state, then the previous states restored. Under the
                     mute button's mute "microphones" only gets "on", which must come back off.
                     The Zigbee role select only gets its current option (another one restarts
                     the daemon and writes the radio), the Zigbee buttons are not pressed, auto
                     update is only set to its current state.
  scenario-persist   --count N (default 20) commands to the number --entity (led_speed or
                     clock_text_speed), --interval-ms apart (default 50), each checked on its
                     echo; the last value is printed and left set (state.conf debounce: one
                     write ~1 s after the last change). No volume or mute command is sent.
                     --install: then an update check and the install of the release offered,
                     waited for up to --install-s (default 300); the daemon exits after it.
                     Refused on the live API port 6053 (it would replace the installed daemon)
                     unless --allow-live-port is given; use a test instance (16053).
The LED ring and clock states (LISTENING, THINKING, SPEAKING, TIMER_RING, the timer) are read on
the station meanwhile (sysfs); the --*-s pauses leave time for that.

Exit status 1 when an expected echo or the voice stop did not come.
Never point it at a station Home Assistant is using (AGENTS.md, docs/stations.md).
"""

import argparse
import asyncio
import dataclasses
import enum
import sys
import time

from aioesphomeapi import (
    APIClient,
    MediaPlayerCommand,
    MediaPlayerState,
    UpdateCommand,
    VoiceAssistantEventType,
    VoiceAssistantTimerEventType,
)

MASKED_DEVICE_INFO = {"project_version", "compilation_time", "mac_address"}
# object_id -> masked fields of its state
MASKED_STATES = {
    "ip_address": {"state", "missing_state"},
    "wifi_signal": {"state", "missing_state"},
    "version": {"state"},
    "uptime": {"state"},
    "illuminance": {"state"},
}
FAILURES = []


def fmt(v):
    if isinstance(v, float):
        return f"{v:.6g}"
    if isinstance(v, enum.Enum):
        return f"{type(v).__name__}.{v.name}"
    if dataclasses.is_dataclass(v) and not isinstance(v, type):
        return "{" + ", ".join(f"{k}={fmt(x)}" for k, x in sorted(vars_of(v).items())) + "}"
    if isinstance(v, (list, tuple)):
        return "[" + ", ".join(fmt(x) for x in v) + "]"
    if isinstance(v, dict):
        return "{" + ", ".join(f"{k}={fmt(x)}" for k, x in sorted(v.items())) + "}"
    return repr(v)


def vars_of(obj):
    return {f.name: getattr(obj, f.name) for f in dataclasses.fields(obj)}


def line(obj, mask=()):
    fields = vars_of(obj)
    return type(obj).__name__ + " " + " ".join(
        f"{k}={'*' if k in mask else fmt(v)}" for k, v in sorted(fields.items())
    )


async def connect(args):
    client = APIClient(args.host, args.port, None, noise_psk=args.psk or None, client_info="esphome_check")
    await client.connect(login=True)
    return client


async def collect_states(client, keys, settle=0.5, limit=5.0):
    """First state of every key, in arrival order (initial states of SubscribeStates)."""
    states = {}
    changed = asyncio.Event()

    def on_state(s):
        if s.key not in states:
            states[s.key] = s
            changed.set()

    client.subscribe_states(on_state)
    loop = asyncio.get_running_loop()
    deadline = loop.time() + limit
    while loop.time() < deadline and not keys <= set(states):
        changed.clear()
        try:
            await asyncio.wait_for(changed.wait(), settle)
        except asyncio.TimeoutError:
            break
    return list(states.values())


async def cmd_dump(client, args):
    info = await client.device_info()
    print(line(info, MASKED_DEVICE_INFO if args.mask else ()))
    entities, services = await client.list_entities_services()
    by_key = {}
    for e in entities:
        print(line(e))
        by_key[e.key] = e
    for s in services:
        print(line(s))
    # Events (button) have no initial state.
    keys = {e.key for e in entities if type(e).__name__ != "EventInfo"}
    for s in await collect_states(client, keys):
        oid = by_key[s.key].object_id if s.key in by_key else "?"
        masked = MASKED_STATES.get(oid, set()) if args.mask else ()
        print(f"{oid}: " + line(s, masked))


async def cmd_commands(client, args):
    entities, services = await client.list_entities_services()
    ents = {e.object_id: e for e in entities}
    queue = asyncio.Queue()
    client.subscribe_states(queue.put_nowait)
    await asyncio.sleep(0.5)
    while not queue.empty():
        queue.get_nowait()

    async def echo(name, oid, send, timeout=2.0, expect=True):
        key = ents[oid].key
        send(key)
        try:
            while True:
                s = await asyncio.wait_for(queue.get(), timeout)
                if s.key == key:
                    print(f"{name}: {line(s)}")
                    if not expect:
                        FAILURES.append(f"{name}: unexpected echo")
                    return
        except asyncio.TimeoutError:
            print(f"{name}: no echo")
            if expect:
                FAILURES.append(f"{name}: no echo")

    await echo("switch", "microphones", lambda k: client.switch_command(k, False))
    await echo("switch", "microphones", lambda k: client.switch_command(k, True))
    await echo("number", "clock_brightness", lambda k: client.number_command(k, 150))
    await echo("number", "led_speed", lambda k: client.number_command(k, 1.5))
    await echo("select", "clock_face", lambda k: client.select_command(k, ents["clock_face"].options[-1]))
    # Not an option of the select: refused, no echo expected.
    await echo(
        "select", "music_visualization", lambda k: client.select_command(k, "no such style"), 1.0, expect=False
    )
    await echo("text", "clock_text", lambda k: client.text_command(k, "Привет"))
    await echo("media", "media_player", lambda k: client.media_player_command(k, volume=0.2))
    await echo("media", "media_player", lambda k: client.media_player_command(k, command=MediaPlayerCommand.PAUSE))
    await echo("light", "ring", lambda k: client.light_command(k, state=True, brightness=0.5, rgb=(1.0, 0.5, 0.0)))
    await echo("light", "ring", lambda k: client.light_command(k, effect=ents["ring"].effects[-1]))
    # Joins whatever Zigbee network permits joining when the daemon runs the router role.
    if "zigbee_join" in ents and args.zigbee_join:
        await echo("button", "zigbee", lambda k: client.button_command(ents["zigbee_join"].key))
    client.update_command(ents["firmware"].key, UpdateCommand.CHECK)
    print("update: check sent")
    svc = {s.name: s for s in services}
    if "clock_show_text" in svc:
        await client.execute_service(svc["clock_show_text"], {"text": "Hi", "seconds": 5})
        print("action: clock_show_text sent")
    if "led_show" in svc:
        await client.execute_service(svc["led_show"], {"effect": "None", "color": "#00ff00", "seconds": 3})
        print("action: led_show sent")
    cfg = await client.get_voice_assistant_configuration(5.0)
    print("voice config: " + line(cfg))
    await asyncio.sleep(0.5)


async def cmd_voice_fake(client, args):
    done = asyncio.Event()
    audio = {"chunks": 0, "bytes": 0}

    async def start(conversation_id, flags, settings, wake_word_phrase):
        print(f"start: phrase={wake_word_phrase!r} flags={flags} settings={fmt(settings)}")
        E = VoiceAssistantEventType

        async def pipeline():
            await asyncio.sleep(0.2)
            client.send_voice_assistant_event(E.VOICE_ASSISTANT_RUN_START, None)
            await asyncio.sleep(0.8)
            client.send_voice_assistant_event(E.VOICE_ASSISTANT_STT_END, {"text": "test"})
            client.send_voice_assistant_event(E.VOICE_ASSISTANT_INTENT_END, {"conversation_id": "c1"})
            client.send_voice_assistant_event(E.VOICE_ASSISTANT_TTS_END, {"url": args.tts_url})
            client.send_voice_assistant_event(E.VOICE_ASSISTANT_RUN_END, None)
            print("events: RUN_START STT_END INTENT_END TTS_END RUN_END sent")

        asyncio.get_running_loop().create_task(pipeline())
        return 0  # audio over the API

    async def stop(abort):
        print(f"stop: abort={abort} audio chunks={audio['chunks']} bytes={audio['bytes']}")
        done.set()

    async def handle_audio(data, *_):
        audio["chunks"] += 1
        audio["bytes"] += len(data)

    client.subscribe_voice_assistant(handle_start=start, handle_stop=stop, handle_audio=handle_audio)
    try:
        await asyncio.wait_for(done.wait(), args.seconds)
    except asyncio.TimeoutError:
        print(f"no stop within {args.seconds} s; audio chunks={audio['chunks']} bytes={audio['bytes']}")
        FAILURES.append("voice: no stop")
    await asyncio.sleep(1.5)


async def cmd_announce(client, args):
    # AnnounceFinished goes to the voice pipeline owner only: subscribe first, as HA does.
    async def start(*_):
        return 0

    async def stop(*_):
        pass

    async def handle_audio(*_):
        pass

    client.subscribe_voice_assistant(handle_start=start, handle_stop=stop, handle_audio=handle_audio)
    await asyncio.sleep(0.3)
    resp = await client.send_voice_assistant_announcement_await_response(
        args.media_url, args.seconds, text=args.text, start_conversation=args.start_conversation
    )
    print("announce finished: " + line(resp))


async def cmd_timer(client, args):
    ev = VoiceAssistantTimerEventType["VOICE_ASSISTANT_TIMER_" + args.event.upper()]
    client.send_voice_assistant_timer_event(ev, args.id, args.name, args.seconds, args.seconds, args.event != "finished")
    await asyncio.sleep(0.5)
    print(f"timer {args.event} sent")


async def cmd_set_key(client, args):
    ok = await client.noise_encryption_set_key(args.key.encode())
    print(f"set-key: success={ok}")

class Watch:
    """Every state the device sends, per key, with a counter to wait for a new one."""

    def __init__(self, client):
        self.latest = {}
        self.history = {}
        self.seq = {}
        self.event = asyncio.Event()
        client.subscribe_states(self._on_state)

    def _on_state(self, s):
        self.latest[s.key] = s
        self.history.setdefault(s.key, []).append(s)
        self.seq[s.key] = self.seq.get(s.key, 0) + 1
        self.event.set()

    async def until(self, key, pred, timeout, after=None):
        """First state of `key` matching `pred` (only states after the `after` count if given)."""
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while True:
            s = self.latest.get(key)
            if s is not None and (after is None or self.seq.get(key, 0) > after) and pred(s):
                return s
            left = deadline - loop.time()
            if left <= 0:
                return None
            self.event.clear()
            try:
                await asyncio.wait_for(self.event.wait(), left)
            except asyncio.TimeoutError:
                return None


async def entities_of(client):
    entities, services = await client.list_entities_services()
    return {e.object_id: e for e in entities}, {s.name: s for s in services}


async def expect(w, name, key, send, pred, timeout=3.0):
    """Sends, then waits for a new state of `key` matching `pred`."""
    before = w.seq.get(key, 0)
    send()
    s = await w.until(key, pred, timeout, after=before)
    if s is None:
        last = w.latest.get(key)
        print(f"{name}: FAIL (last: {line(last) if last else 'none'})")
        FAILURES.append(name)
    else:
        print(f"{name}: {line(s)}")
    return s


async def cmd_scenario_voice(client, args):
    w = Watch(client)
    ents, _ = await entities_of(client)
    media = ents["media_player"].key
    E = VoiceAssistantEventType
    starts = []
    stops = []
    audio = {"chunks": 0}
    run_done = asyncio.Event()

    async def pipeline():
        await asyncio.sleep(0.3)
        client.send_voice_assistant_event(E.VOICE_ASSISTANT_RUN_START, None)
        print(f"  RUN_START; listening {args.listen_s} s")
        await asyncio.sleep(args.listen_s)
        client.send_voice_assistant_event(E.VOICE_ASSISTANT_STT_END, {"text": "test"})
        client.send_voice_assistant_event(E.VOICE_ASSISTANT_INTENT_START, None)
        print(f"  STT_END INTENT_START; thinking {args.think_s} s (audio chunks so far {audio['chunks']})")
        await asyncio.sleep(args.think_s)
        client.send_voice_assistant_event(
            E.VOICE_ASSISTANT_INTENT_END, {"conversation_id": "c1", "continue_conversation": "0"}
        )
        client.send_voice_assistant_event(E.VOICE_ASSISTANT_TTS_START, {"text": "ok"})
        client.send_voice_assistant_event(E.VOICE_ASSISTANT_TTS_END, {"url": args.tts_url})
        client.send_voice_assistant_event(E.VOICE_ASSISTANT_RUN_END, None)
        print("  INTENT_END TTS_START TTS_END RUN_END; speaking")
        run_done.set()

    async def start(conversation_id, flags, settings, wake_word_phrase):
        starts.append(wake_word_phrase)
        print(f"start #{len(starts)}: phrase={wake_word_phrase!r} flags={flags}")
        if len(starts) == 1:
            asyncio.get_running_loop().create_task(pipeline())
        return 0

    async def stop(abort):
        stops.append(abort)
        print(f"stop: abort={abort}")

    async def handle_audio(data, *_):
        audio["chunks"] += 1

    client.subscribe_voice_assistant(handle_start=start, handle_stop=stop, handle_audio=handle_audio)
    await asyncio.sleep(0.5)
    print(f"announce {args.media_url} with start_conversation")
    resp = await client.send_voice_assistant_announcement_await_response(
        args.media_url, args.seconds, text="scenario-voice", start_conversation=True
    )
    print("announce finished: " + line(resp))
    if not resp.success:
        FAILURES.append("voice: announcement did not play to its end")
    seen = [s.state for s in w.history.get(media, [])]
    if MediaPlayerState.ANNOUNCING not in seen:
        FAILURES.append("voice: media player never ANNOUNCING")
    try:
        await asyncio.wait_for(run_done.wait(), 5.0 + args.listen_s + args.think_s)
    except asyncio.TimeoutError:
        FAILURES.append("voice: no pipeline start after the announcement")
        return
    await asyncio.sleep(args.speak_s)
    print(f"after the answer: starts={len(starts)} stops={stops} audio chunks={audio['chunks']}")
    if starts and starts[0]:
        FAILURES.append(f"voice: continued conversation started with phrase {starts[0]!r}")
    if len(starts) != 1:
        FAILURES.append(f"voice: {len(starts)} pipeline starts, expected 1")
    if audio["chunks"] == 0:
        FAILURES.append("voice: no microphone audio while listening")
    s = w.latest.get(media)
    print("media player: " + (line(s) if s else "none"))
    if s is None or s.state != MediaPlayerState.IDLE:
        FAILURES.append("voice: media player not IDLE at the end")


async def cmd_scenario_timer(client, args):
    T = VoiceAssistantTimerEventType
    total = args.seconds
    client.send_voice_assistant_timer_event(T.VOICE_ASSISTANT_TIMER_STARTED, args.id, args.name, total, total, True)
    print(f"timer STARTED {total} s; holding {args.hold_s} s (clock counts down)")
    await asyncio.sleep(args.hold_s)
    left = max(total - int(args.hold_s), 1)
    client.send_voice_assistant_timer_event(T.VOICE_ASSISTANT_TIMER_UPDATED, args.id, args.name, total, left, True)
    await asyncio.sleep(1.0)
    client.send_voice_assistant_timer_event(T.VOICE_ASSISTANT_TIMER_FINISHED, args.id, args.name, total, 0, False)
    print(f"timer FINISHED; ringing, waiting {args.ring_s} s (stop the instance to end the ring)")
    await asyncio.sleep(args.ring_s)


async def cmd_scenario_media(client, args):
    w = Watch(client)
    ents, _ = await entities_of(client)
    key = ents["media_player"].key
    await asyncio.sleep(0.5)
    start = w.latest.get(key)
    restore = min(start.volume if start else 0.2, 0.2)
    S = MediaPlayerState
    C = MediaPlayerCommand

    def cmd(**kw):
        return lambda: client.media_player_command(key, **kw)

    await expect(w, "media: play url", key, cmd(media_url=args.media_url), lambda s: s.state == S.PLAYING, 15)
    await asyncio.sleep(args.hold_s)
    await expect(w, "media: pause", key, cmd(command=C.PAUSE), lambda s: s.state == S.PAUSED, 5)
    await expect(w, "media: resume", key, cmd(command=C.PLAY), lambda s: s.state == S.PLAYING, 5)
    await expect(w, "media: volume 0.1", key, cmd(volume=0.1), lambda s: abs(s.volume - 0.1) < 0.01)
    await expect(w, "media: mute", key, cmd(command=C.MUTE), lambda s: s.muted)
    await expect(w, "media: unmute", key, cmd(command=C.UNMUTE), lambda s: not s.muted)
    await expect(w, f"media: volume {restore:.2f}", key, cmd(volume=restore), lambda s: abs(s.volume - restore) < 0.01)
    await expect(w, "media: stop", key, cmd(command=C.STOP), lambda s: s.state == S.IDLE, 5)


async def cmd_scenario_entities(client, args):
    w = Watch(client)
    ents, svcs = await entities_of(client)
    await asyncio.sleep(0.8)
    k = {oid: e.key for oid, e in ents.items()}
    orig = {oid: w.latest.get(key) for oid, key in k.items()}
    restore = []

    def st(oid):
        return orig[oid].state if orig.get(oid) is not None else None

    switches = ["microphones", "clock_display", "nearest_station_answers", "voice_indication"]
    if st("hardware_mic_mute"):
        # Under the mute button's mute "on" is refused: the switch comes back off.
        await expect(w, "switch microphones -> True (refused: mute button)", k["microphones"], lambda: client.switch_command(k["microphones"], True), lambda s: not s.state)
        switches.remove("microphones")
    for oid in switches:
        cur = bool(st(oid))
        await expect(w, f"switch {oid} -> {not cur}", k[oid], lambda o=oid, v=not cur: client.switch_command(k[o], v), lambda s, v=not cur: s.state == v)
        restore.append((f"switch {oid} -> {cur}", k[oid], lambda o=oid, v=cur: client.switch_command(k[o], v), lambda s, v=cur: s.state == v))
    cur = bool(st("auto_update"))
    await expect(w, f"switch auto_update -> {cur} (unchanged)", k["auto_update"], lambda: client.switch_command(k["auto_update"], cur), lambda s: s.state == cur)

    for oid, value, want in [
        ("clock_brightness", 150, 100.0),
        ("clock_brightness", 42.0, 42.0),
        ("clock_text_speed", 333, 350.0),
        ("led_speed", 9.0, 4.0),
        ("led_speed", 1.5, 1.5),
    ]:
        await expect(w, f"number {oid} {value} -> {want}", k[oid], lambda o=oid, v=value: client.number_command(k[o], v), lambda s, x=want: abs(s.state - x) < 1e-3)
    for oid in ["clock_brightness", "clock_text_speed", "led_speed"]:
        if st(oid) is not None:
            v = st(oid)
            restore.append((f"number {oid} -> {v}", k[oid], lambda o=oid, x=v: client.number_command(k[o], x), lambda s, x=v: abs(s.state - x) < 1e-3))

    for oid in ["clock_face", "music_visualization", "case_color", "clock_music"]:
        opts = ents[oid].options
        cur = st(oid)
        other = next((o for o in reversed(opts) if o != cur), cur)
        await expect(w, f"select {oid} -> {other}", k[oid], lambda o=oid, v=other: client.select_command(k[o], v), lambda s, v=other: s.state == v)
        if cur is not None:
            restore.append((f"select {oid} -> {cur}", k[oid], lambda o=oid, v=cur: client.select_command(k[o], v), lambda s, v=cur: s.state == v))
    if "zigbee_role" in k:
        cur = st("zigbee_role")
        await expect(w, f"select zigbee_role -> {cur} (unchanged)", k["zigbee_role"], lambda: client.select_command(k["zigbee_role"], cur), lambda s: s.state == cur)
        z = w.latest.get(k.get("zigbee"))
        print("text sensor zigbee: " + (line(z) if z else "none"))

    text = st("clock_text") or ""
    await expect(w, "text clock_text -> Тест", k["clock_text"], lambda: client.text_command(k["clock_text"], "Тест"), lambda s: s.state == "Тест")
    restore.append((f"text clock_text -> {text!r}", k["clock_text"], lambda: client.text_command(k["clock_text"], text), lambda s: s.state == text))

    ring = orig.get("ring")
    await expect(w, "light ring on", k["ring"], lambda: client.light_command(k["ring"], state=True, brightness=0.4, rgb=(0.0, 0.5, 1.0)), lambda s: s.state and abs(s.brightness - 0.4) < 0.01)
    await expect(w, "light ring effect", k["ring"], lambda: client.light_command(k["ring"], effect=ents["ring"].effects[-1]), lambda s: s.effect == ents["ring"].effects[-1])
    if ring is not None:
        restore.append((
            "light ring restored",
            k["ring"],
            lambda: client.light_command(k["ring"], state=ring.state, brightness=ring.brightness, rgb=(ring.red, ring.green, ring.blue), effect=ring.effect or "None"),
            lambda s: s.state == ring.state,
        ))

    for name, data in [
        ("clock_show_text", {"text": "Hi", "seconds": 3}),
        ("clock_show_face", {"face": ents["clock_face"].options[-1], "seconds": 3}),
        ("led_show", {"effect": "", "color": "#00ff00", "seconds": 3}),
        ("led_show", {"effect": "", "color": "", "seconds": 0}),
    ]:
        if name in svcs:
            await client.execute_service(svcs[name], data)
            print(f"action {name} {data} sent")
            await asyncio.sleep(0.5)
        else:
            FAILURES.append(f"action {name} missing")

    for name, key, send, pred in restore:
        await expect(w, name, key, send, pred)


# Values the daemon keeps as sent (inside its clamp, clock_text_speed in steps of 50).
PERSIST_VALUES = {
    "led_speed": [round(0.5 + 0.1 * i, 1) for i in range(36)],
    "clock_text_speed": [100.0 + 50 * i for i in range(39)],
}


async def cmd_scenario_persist(client, args):
    w = Watch(client)
    ents, _ = await entities_of(client)
    await asyncio.sleep(0.8)
    key = ents[args.entity].key
    values = PERSIST_VALUES[args.entity]
    v = None
    for i in range(args.count):
        v = values[i % len(values)]
        await expect(
            w,
            f"number {args.entity} #{i + 1} -> {v:g}",
            key,
            lambda x=v: client.number_command(key, x),
            lambda s, x=v: abs(s.state - x) < 1e-3,
        )
        await asyncio.sleep(args.interval_ms / 1000)
    if v is not None:
        print(f"last {args.entity}={v:g} echoed at {time.strftime('%H:%M:%S')}")
    if not args.install:
        return
    # update_attempt=<tag> is written at once when the install starts; the install ends with the
    # daemon's exit (a test instance is not restarted: no supervisor).
    fw = ents["firmware"].key
    offer = await expect(
        w,
        "update: check",
        fw,
        lambda: client.update_command(fw, UpdateCommand.CHECK),
        lambda s: s.latest_version and s.latest_version != s.current_version,
        30,
    )
    if offer is None:
        return
    started = await expect(
        w,
        f"update: install {offer.latest_version} at {time.strftime('%H:%M:%S')}",
        fw,
        lambda: client.update_command(fw, UpdateCommand.INSTALL),
        lambda s: s.in_progress,
        10,
    )
    if started is None:
        return
    done = await w.until(fw, lambda s: not s.in_progress or s.progress >= 100, args.install_s, after=w.seq[fw])
    print(f"update: {line(done) if done else f'no end within {args.install_s} s'} at {time.strftime('%H:%M:%S')}")
    if done is None or not done.in_progress:
        FAILURES.append("update install")


COMMANDS = {
    "dump": cmd_dump,
    "commands": cmd_commands,
    "voice-fake": cmd_voice_fake,
    "announce": cmd_announce,
    "timer": cmd_timer,
    "set-key": cmd_set_key,
    "scenario-voice": cmd_scenario_voice,
    "scenario-timer": cmd_scenario_timer,
    "scenario-media": cmd_scenario_media,
    "scenario-entities": cmd_scenario_entities,
    "scenario-persist": cmd_scenario_persist,
}


# The running daemon's ESPHome API port (a test instance listens at PULSAR_PORT_OFFSET + 6053).
LIVE_API_PORT = 6053


async def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--host", required=True)
    p.add_argument("--port", type=int, default=6053)
    p.add_argument("--psk", help="Noise key, base64 (omit for plaintext)")
    p.add_argument("command", choices=COMMANDS)
    p.add_argument("--mask", action="store_true")
    p.add_argument("--tts-url", default="http://192.168.1.50:8123/api/tts_proxy/test.mp3")
    p.add_argument("--media-url", default="http://192.168.1.50:8123/local/test.mp3")
    p.add_argument("--text", default="")
    p.add_argument("--start-conversation", action="store_true")
    p.add_argument("--event", default="started", choices=["started", "updated", "cancelled", "finished"])
    p.add_argument("--seconds", type=int, help="default 10; scenario-timer 70, scenario-voice 30")
    p.add_argument("--id", default="t1")
    p.add_argument("--name", default="test")
    p.add_argument("--key", default="")
    p.add_argument("--zigbee-join", action="store_true", help="commands: press Zigbee join (router role joins)")
    p.add_argument("--listen-s", type=float, default=3.0)
    p.add_argument("--think-s", type=float, default=3.0)
    p.add_argument("--speak-s", type=float, default=8.0)
    p.add_argument("--hold-s", type=float, default=5.0)
    p.add_argument("--ring-s", type=float, default=5.0)
    p.add_argument("--entity", default="led_speed", choices=sorted(PERSIST_VALUES))
    p.add_argument("--count", type=int, default=20)
    p.add_argument("--interval-ms", type=int, default=50)
    p.add_argument("--install", action="store_true", help="scenario-persist: then check and install an update")
    p.add_argument("--install-s", type=float, default=300.0)
    p.add_argument("--allow-live-port", action="store_true", help="scenario-persist --install against port 6053")
    args = p.parse_args()
    if args.command == "scenario-persist" and args.install and args.port == LIVE_API_PORT and not args.allow_live_port:
        p.error(f"--install on the live API port {LIVE_API_PORT} replaces the installed daemon; "
                "use a test instance or pass --allow-live-port")
    if args.seconds is None:
        args.seconds = {"scenario-timer": 70, "scenario-voice": 30}.get(args.command, 10)
    client = await connect(args)
    try:
        await COMMANDS[args.command](client, args)
    finally:
        await client.disconnect()
    for f in FAILURES:
        print(f"FAIL {f}", file=sys.stderr)
    return 1 if FAILURES else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
