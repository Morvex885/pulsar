#!/usr/bin/env python3
"""Sendspin test server for pulsar.

Drives a Sendspin *client* (the speaker, e.g. sendspin-cpp inside `sendspin_probe`) the way
Music Assistant does: the server dials the client's WebSocket endpoint
(`ws://<host>:<port>/sendspin`, server-initiated connection), runs the Noise handshake with
the sentinel PSK (unpaired access) and then:

  1. prints the client's hello (client_id, name, roles, supported formats),
  2. streams a gentle 440 Hz sine (-20 dBFS) once per codec in --codecs (FLAC, then PCM by
     default), with the group STOPPED for ~2 s between the runs,
  3. sends player volume/mute commands during the first stream (never above 20 %),
  4. publishes group/update playing/stopped around every stream,
  5. advertises play/pause/stop/next/previous in the controller state and records every
     client/command the client sends,
  6. lets aiosendspin's visualizer role send the audio-derived frames the client requests
     (loudness, f_peak, spectrum, peak), plus beat frames from a synthetic 120 BPM beat
     schedule (aiosendspin has no beat detector; beats come from the application),
  7. stops, disconnects and prints a summary.

Built on aiosendspin's server API (the library Music Assistant uses), which needs the
`server` extra: `aiosendspin[server]` (PyAV, numpy, pillow). No mDNS advertising or
discovery is started: the server never listens, it only dials the given client URL.

Example:
    sendspin_server.py 192.168.1.50 --port 18928 --seconds 10 --volume 15
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import math
import sys
import time
from array import array
from collections import Counter
from collections.abc import Callable
from dataclasses import dataclass, field
from importlib.metadata import version
from typing import Any

from aiosendspin.models.core import (
    ClientCommandMessage,
    ClientGoodbyeMessage,
    ClientStateMessage,
    ServerCommandMessage,
    ServerMessage,
)
from aiosendspin.models.types import (
    AudioCodec,
    BinaryMessageType,
    ConnectionReason,
    MediaCommand,
    PlayerCommand,
)
from aiosendspin.models.visualizer import BeatTiming
from aiosendspin.noise import Identity, InMemoryServerPairingStore, TrustedUnpairedClient
from aiosendspin.server import (
    AudioFormat,
    ClientAddedEvent,
    ClientDisconnectedEvent,
    GroupStateChangedEvent,
    SendspinClient,
    SendspinEvent,
    SendspinGroup,
    SendspinServer,
)
from aiosendspin.server.connection import SendspinConnection
from aiosendspin.server.events import GroupEvent
from aiosendspin.server.roles.controller import ControllerEvent, ControllerGroupRole
from aiosendspin.server.roles.player import PlayerV1Role
from aiosendspin.server.roles.visualizer import VisualizerGroupRole, VisualizerV1Role

AIOSENDSPIN_VERSION = version("aiosendspin")

# Hard ceiling for any player volume this script lets out on the wire (percent).
MAX_VOLUME = 20

SAMPLE_RATE = 48_000
CHANNELS = 2
BIT_DEPTH = 16
TONE_HZ = 440
TONE_DBFS = -20.0
# 11 periods of 440 Hz are exactly 1200 frames (25 ms) at 48 kHz, so repeating one chunk
# yields a phase-continuous tone.
CHUNK_FRAMES = 1200
CHUNK_US = CHUNK_FRAMES * 1_000_000 // SAMPLE_RATE
# How far ahead of real time the stream may run (the clients buffer it).
MAX_AHEAD_US = 1_000_000
GAP_S = 2.0
BEAT_INTERVAL_US = 500_000  # synthetic 120 BPM beat schedule for the visualizer

CODECS = {"flac": AudioCodec.FLAC, "pcm": AudioCodec.PCM, "opus": AudioCodec.OPUS}
CONTROLLER_COMMANDS = [
    MediaCommand.PLAY,
    MediaCommand.PAUSE,
    MediaCommand.STOP,
    MediaCommand.NEXT,
    MediaCommand.PREVIOUS,
]
BINARY_NAMES = {
    BinaryMessageType.AUDIO_CHUNK.value: "audio",
    BinaryMessageType.VISUALIZATION_LOUDNESS.value: "loudness",
    BinaryMessageType.VISUALIZATION_BEAT.value: "beat",
    BinaryMessageType.VISUALIZATION_F_PEAK.value: "f_peak",
    BinaryMessageType.VISUALIZATION_SPECTRUM.value: "spectrum",
    BinaryMessageType.VISUALIZATION_PEAK.value: "peak",
    BinaryMessageType.VISUALIZATION_PITCH.value: "pitch",
}

_T0 = time.monotonic()


def log(message: str) -> None:
    """Print one line prefixed with seconds since script start (monotonic clock)."""
    print(f"[{time.monotonic() - _T0:8.3f}] {message}", flush=True)


class AutoTrustPairingStore(InMemoryServerPairingStore):
    """In-memory pairing store that approves every client for unpaired playback.

    aiosendspin only activates roles on an unpaired (sentinel PSK) connection when the
    operator trusted the client_id beforehand. The client_id is only known from the hello,
    so this test server trusts any client it dials; nothing is persisted.
    """

    async def trusted_unpaired(self, client_id: str) -> TrustedUnpairedClient | None:
        trusted = await super().trusted_unpaired(client_id)
        return trusted if trusted is not None else TrustedUnpairedClient(client_id=client_id)


@dataclass
class WireRecord:
    """What actually crossed the WebSocket, as seen by the wire tap."""

    run: str = "setup"
    # run label -> list of stream/start player objects sent during that run
    player_stream_starts: dict[str, list[dict[str, Any]]] = field(default_factory=dict)
    # (run label, binary kind) -> frames sent
    binary: Counter[tuple[str, str]] = field(default_factory=Counter)
    volumes_sent: list[int] = field(default_factory=list)
    mutes_sent: list[bool] = field(default_factory=list)
    blocked_volumes: list[int] = field(default_factory=list)
    group_states: list[str] = field(default_factory=list)
    controller_supported: list[str] | None = None
    controller_commands: list[str] = field(default_factory=list)


class WireTap:
    """Observe outgoing/incoming messages of every aiosendspin server connection.

    aiosendspin has no public hook for the messages a connection writes, so this wraps the
    connection's private send/dispatch methods. It only logs and counts; the single
    intervention is a guard that drops any player volume command above MAX_VOLUME.
    """

    def __init__(self) -> None:
        self.record = WireRecord()
        self._last_client_player: tuple[Any, Any] | None = None
        self._last_client_visualizer: Any = None

    def install(self) -> None:
        tap = self
        send_message = SendspinConnection._send_message  # noqa: SLF001
        send_binary = SendspinConnection._send_binary_data  # noqa: SLF001
        handle_message = SendspinConnection._handle_message  # noqa: SLF001

        async def tapped_send_message(conn: Any, wsock: Any, message: ServerMessage) -> None:
            if tap.blocks(message):
                return
            await send_message(conn, wsock, message)
            tap.on_sent(message)

        async def tapped_send_binary(
            conn: Any, wsock: Any, role: str, entry: Any, buffer_tracker: Any
        ) -> None:
            await send_binary(conn, wsock, role, entry, buffer_tracker)
            kind = BINARY_NAMES.get(entry.binary.message_type, f"type{entry.binary.message_type}")
            tap.record.binary[(tap.record.run, kind)] += 1

        async def tapped_handle_message(conn: Any, message: Any, timestamp_us: int) -> None:
            tap.on_received(message)
            await handle_message(conn, message, timestamp_us)

        SendspinConnection._send_message = tapped_send_message  # type: ignore[method-assign]  # noqa: SLF001
        SendspinConnection._send_binary_data = tapped_send_binary  # type: ignore[method-assign]  # noqa: SLF001
        SendspinConnection._handle_message = tapped_handle_message  # type: ignore[method-assign]  # noqa: SLF001

    def blocks(self, message: ServerMessage) -> bool:
        """Drop a player volume command above MAX_VOLUME, whoever produced it."""
        if not isinstance(message, ServerCommandMessage):
            return False
        player = message.payload.player
        if player is None or player.command != PlayerCommand.VOLUME:
            return False
        if player.volume is not None and player.volume > MAX_VOLUME:
            self.record.blocked_volumes.append(player.volume)
            log(f"BLOCKED server/command player volume={player.volume} (> {MAX_VOLUME})")
            return True
        return False

    def on_sent(self, message: ServerMessage) -> None:
        data = json.loads(message.to_json())
        kind = data.get("type")
        payload = data.get("payload") or {}
        if kind == "server/time":
            return
        rec = self.record
        if kind == "server/activate":
            log(
                f"sent server/activate activities={payload.get('activities')} "
                f"active_roles={payload.get('active_roles')}"
            )
        elif kind == "stream/start":
            for role, obj in payload.items():
                if role == "server_transmitted" or not isinstance(obj, dict):
                    continue
                if role == "player":
                    rec.player_stream_starts.setdefault(rec.run, []).append(obj)
                    header = obj.get("codec_header")
                    log(
                        f"sent stream/start player codec={obj.get('codec')} "
                        f"rate={obj.get('sample_rate')} channels={obj.get('channels')} "
                        f"bit_depth={obj.get('bit_depth')}"
                        + (f" codec_header={len(header)}b64-chars" if header else "")
                    )
                else:
                    log(f"sent stream/start {role} {json.dumps(obj, sort_keys=True)}")
        elif kind in ("stream/end", "stream/clear"):
            log(f"sent {kind} roles={payload.get('roles')}")
        elif kind == "group/update":
            state = payload.get("playback_state")
            if state is not None:
                rec.group_states.append(state)
            log(
                f"sent group/update group_id={payload.get('group_id')} "
                f"name={payload.get('group_name')!r} state={state}"
            )
        elif kind == "server/command":
            player = payload.get("player") or {}
            if player.get("command") == "volume":
                rec.volumes_sent.append(player["volume"])
                log(f"sent server/command player volume={player['volume']}")
            elif player.get("command") == "mute":
                rec.mutes_sent.append(player["mute"])
                log(f"sent server/command player mute={str(player['mute']).lower()}")
            else:
                log(f"sent server/command {json.dumps(payload, sort_keys=True)}")
        elif kind == "server/state":
            controller = payload.get("controller")
            if controller is not None:
                rec.controller_supported = controller.get("supported_commands")
                log(
                    "sent server/state controller "
                    f"supported_commands={controller.get('supported_commands')} "
                    f"volume={controller.get('volume')} muted={controller.get('muted')}"
                )
            others = sorted(k for k in payload if k != "controller")
            if others:
                log(f"sent server/state {others}")
        else:
            log(f"sent {kind}")

    def on_received(self, message: Any) -> None:
        if isinstance(message, ClientCommandMessage):
            controller = message.payload.controller
            if controller is not None:
                command = controller.command.value
                self.record.controller_commands.append(command)
                log(f"controller command received: {command}")
            else:
                log(f"client/command received without controller object: {message.payload}")
        elif isinstance(message, ClientStateMessage):
            payload = message.payload
            player = payload.player
            if player is not None:
                current = (player.volume, player.muted)
                if current != self._last_client_player:
                    self._last_client_player = current
                    log(f"client/state player volume={player.volume} muted={player.muted}")
            vis = payload.visualizer
            if vis is not None and vis != self._last_client_visualizer:
                self._last_client_visualizer = vis
                spectrum = vis.spectrum
                log(
                    f"client/state visualizer types={list(vis.types)} rate_max={vis.rate_max}"
                    + (
                        f" spectrum={spectrum.n_disp_bins} bins {spectrum.scale} "
                        f"{spectrum.f_min}-{spectrum.f_max} Hz"
                        if spectrum is not None
                        else ""
                    )
                )
        elif isinstance(message, ClientGoodbyeMessage):
            log(f"client/goodbye reason={message.payload.reason}")


def tone_chunk() -> bytes:
    """One 25 ms chunk of a 440 Hz stereo sine at TONE_DBFS, 16-bit little-endian PCM."""
    amplitude = 32767 * 10 ** (TONE_DBFS / 20)
    samples = array("h")
    for n in range(CHUNK_FRAMES):
        value = round(amplitude * math.sin(2 * math.pi * TONE_HZ * n / SAMPLE_RATE))
        samples.extend((value,) * CHANNELS)
    if sys.byteorder != "little":
        samples.byteswap()
    return samples.tobytes()


def print_hello(client: SendspinClient) -> None:
    info = client.info
    log(f"client hello: client_id={client.client_id} name={info.name!r}")
    log(f"  supported_roles={info.supported_roles}")
    if info.device_info is not None:
        log(f"  device_info={info.device_info}")
    if info.player_support is not None:
        support = info.player_support
        for fmt in support.supported_formats:
            log(
                f"  player format: {fmt.codec.value} {fmt.channels}ch "
                f"{fmt.sample_rate} Hz {fmt.bit_depth} bit"
            )
        log(f"  player buffer_capacity={support.buffer_capacity}")
    if info.visualizer_support is not None:
        log(f"  visualizer buffer_capacity={info.visualizer_support.buffer_capacity}")
    if info.supported_pair_methods is not None:
        log(f"  supported_pair_methods={info.supported_pair_methods}")
    log(f"  unpaired_access.enabled={info.unpaired_access.enabled}")
    security = client.connection_security
    if security is not None:
        log(f"  connection psk_category={security.psk_category.value}")
    log(f"  active roles: {client.active_role_ids}")


async def volume_mute_sequence(
    player: PlayerV1Role, volume: int, interval_s: float, done: list[str]
) -> None:
    """Send volume, volume - 5, mute on, mute off, `interval_s` apart."""
    steps: list[tuple[str, Callable[[], None], PlayerCommand]] = [
        (f"volume {volume}", lambda: player.set_volume(volume), PlayerCommand.VOLUME),
        (
            f"volume {max(volume - 5, 0)}",
            lambda: player.set_volume(max(volume - 5, 0)),
            PlayerCommand.VOLUME,
        ),
        ("mute on", lambda: player.set_mute(True), PlayerCommand.MUTE),
        ("mute off", lambda: player.set_mute(False), PlayerCommand.MUTE),
    ]
    for label, action, command in steps:
        await asyncio.sleep(interval_s)
        if command not in player.state_supported_commands:
            log(
                f"player command {label}: client did not declare '{command.value}' in its "
                f"client/state supported_commands {[c.value for c in player.state_supported_commands]}"
                " - not sent"
            )
            continue
        log(f"player command: {label}")
        action()
        done.append(label)


async def stream_tone(
    *,
    run: str,
    codec: AudioCodec,
    seconds: float,
    client: SendspinClient,
    group: SendspinGroup,
    player: PlayerV1Role,
    tap: WireTap,
    volume: int | None,
) -> None:
    """Stream the tone with `codec` for `seconds`, then stop the group."""
    audio_format = AudioFormat(sample_rate=SAMPLE_RATE, bit_depth=BIT_DEPTH, channels=CHANNELS)
    if not player.set_preferred_format(audio_format, codec):
        log(
            f"{run}: client does not accept {codec.value} {SAMPLE_RATE}/{BIT_DEPTH}/{CHANNELS}"
            " (not in its supported_formats or not encodable) - skipped"
        )
        return
    log(
        f"{run}: preferred format set to {codec.value} {SAMPLE_RATE} Hz {BIT_DEPTH} bit "
        f"{CHANNELS}ch (player.preferred_codec={player.preferred_codec.value if player.preferred_codec else None})"
    )
    tap.record.run = run
    chunk = tone_chunk()
    n_chunks = max(1, round(seconds * 1_000_000 / CHUNK_US))
    visualizer_group = group.group_role("visualizer")

    log(f"{run}: start_stream, group state -> playing")
    stream = group.start_stream()
    commands_task: asyncio.Task[None] | None = None
    commands_done: list[str] = []
    if volume is not None:
        interval_s = max(0.5, min(2.0, seconds / 5))
        commands_task = asyncio.create_task(
            volume_mute_sequence(player, volume, interval_s, commands_done)
        )

    last_start_us = 0
    for index in range(n_chunks):
        if not client.is_connected:
            log(f"{run}: client disconnected, aborting stream")
            break
        stream.prepare_audio(chunk, audio_format)
        last_start_us = await stream.commit_audio()
        if index == 0:
            log(f"{run}: first chunk plays at server time {last_start_us} us")
            if isinstance(visualizer_group, VisualizerGroupRole):
                beats = [
                    BeatTiming(timestamp_us=last_start_us + i * BEAT_INTERVAL_US, is_downbeat=i % 4 == 0)
                    for i in range(int(seconds * 1_000_000 // BEAT_INTERVAL_US))
                ]
                visualizer_group.append_beat_schedule(beats)
                log(f"{run}: appended synthetic beat schedule ({len(beats)} beats, 120 BPM)")
        await stream.sleep_to_limit_buffer(MAX_AHEAD_US)

    # Let the client play out what it buffered before ending the stream.
    remaining_s = (last_start_us + CHUNK_US - stream.now_us()) / 1_000_000
    if remaining_s > 0 and client.is_connected:
        await asyncio.sleep(remaining_s)
    if commands_task is not None:
        await commands_task
    log(f"{run}: stop, group state -> stopped")
    await group.stop()
    if isinstance(visualizer_group, VisualizerGroupRole):
        visualizer_group.clear_beat_schedule()
    tap.record.run = f"after {run}"
    counts = {kind: n for (r, kind), n in tap.record.binary.items() if r == run}
    log(f"{run}: frames sent {dict(sorted(counts.items()))}")


async def run(args: argparse.Namespace) -> int:
    log(f"aiosendspin {AIOSENDSPIN_VERSION}")
    tap = WireTap()
    tap.install()
    server = SendspinServer(
        asyncio.get_running_loop(),
        Identity.generate(),
        "pulsar sendspin test server",
        pairing_store=AutoTrustPairingStore(),
    )
    log(f"server id={server.id} (no listener, no mDNS advertising/discovery)")
    url = f"ws://{args.host}:{args.port}/sendspin"
    runs = [f"stream {i + 1} ({name})" for i, name in enumerate(args.codecs)]
    connected = False
    try:
        connected = await exercise(server, url, args, tap, runs)
    finally:
        await disconnect(server)
    return finish(evaluate(connected, args, runs, tap.record), tap.record)


async def exercise(
    server: SendspinServer, url: str, args: argparse.Namespace, tap: WireTap, runs: list[str]
) -> bool:
    """Connect, stream and send commands; return whether the client connected."""
    added: asyncio.Future[str] = asyncio.get_running_loop().create_future()

    def on_server_event(_server: SendspinServer, event: SendspinEvent) -> None:
        if isinstance(event, ClientAddedEvent) and not added.done():
            added.set_result(event.client_id)
        elif isinstance(event, ClientDisconnectedEvent):
            reason = event.goodbye_reason.value if event.goodbye_reason else None
            log(f"client {event.client_id} disconnected (goodbye reason={reason})")

    server.add_event_listener(on_server_event)
    log(f"connecting to {url}")
    try:
        await asyncio.wait_for(
            server.connect_to_client_and_wait(url, connection_reason=ConnectionReason.PLAYBACK),
            timeout=15,
        )
        log("websocket open, waiting for handshake, client/hello and initial client/state")
        client_id = await asyncio.wait_for(added, timeout=15)
    except Exception as err:  # noqa: BLE001
        log(f"connection failed: {type(err).__name__}: {err}")
        return False
    client = server.get_client(client_id)
    assert client is not None
    print_hello(client)

    player = client.role("player@v1")
    if not isinstance(player, PlayerV1Role):
        log(f"client has no active player@v1 role (active: {client.active_role_ids})")
        return True
    visualizer = client.role("visualizer@v1")
    if isinstance(visualizer, VisualizerV1Role):
        visualizer.set_tracks_downbeats(tracks=True)
    else:
        log("visualizer: client has no active visualizer@v1 role")

    group = client.group
    log(f"client is in group {group.group_id} ({len(group.clients)} member(s))")
    group.set_group_name("pulsar test")

    def on_group_event(_group: SendspinGroup, event: GroupEvent) -> None:
        if isinstance(event, GroupStateChangedEvent):
            log(f"group state changed: {event.state.value}")
        elif isinstance(event, ControllerEvent):
            log(f"controller event accepted by aiosendspin: {type(event).__name__}")

    group.add_event_listener(on_group_event)

    controller = group.group_role("controller")
    if isinstance(controller, ControllerGroupRole):
        controller.set_supported_commands(CONTROLLER_COMMANDS)
        log(f"controller supported commands set: {[c.value for c in CONTROLLER_COMMANDS]}")
    else:
        log(f"controller: not supported by aiosendspin {AIOSENDSPIN_VERSION}")

    for index, (name, label) in enumerate(zip(args.codecs, runs, strict=True)):
        if index > 0:
            log(f"gap {GAP_S:.0f} s with group stopped")
            await asyncio.sleep(GAP_S)
        if not client.is_connected:
            log(f"{label}: client not connected, skipped")
            continue
        await stream_tone(
            run=label,
            codec=CODECS[name],
            seconds=args.seconds,
            client=client,
            group=group,
            player=player,
            tap=tap,
            volume=args.volume if index == 0 else None,
        )

    # Make sure the stopped state and stream/end went out before the socket closes.
    if client.is_connected:
        await client.wait_role_drained("player")
        await asyncio.sleep(0.5)
    return True


def evaluate(
    connected: bool, args: argparse.Namespace, runs: list[str], rec: WireRecord
) -> dict[str, bool]:
    """Derive the pass/fail checks from what the wire tap saw."""
    checks = {"connection": connected}
    for name, label in zip(args.codecs, runs, strict=True):
        starts = rec.player_stream_starts.get(label, [])
        checks[label] = (
            bool(starts)
            and all(
                s.get("codec") == name
                and s.get("sample_rate") == SAMPLE_RATE
                and s.get("channels") == CHANNELS
                and s.get("bit_depth") == BIT_DEPTH
                for s in starts
            )
            and rec.binary[(label, "audio")] > 0
        )
    checks["volume commands"] = rec.volumes_sent[:2] == [args.volume, max(args.volume - 5, 0)]
    checks["mute commands"] = rec.mutes_sent[:2] == [True, False]
    checks["group states"] = (
        rec.group_states.count("playing") >= len(runs)
        and rec.group_states.count("stopped") >= len(runs)
        and rec.group_states[-1:] == ["stopped"]
    )
    return checks


async def disconnect(server: SendspinServer) -> None:
    """Close every client connection (no redial), then the server and its HTTP session."""
    for client in server.connected_clients:
        if client.connection is not None:
            await client.connection.disconnect(retry_connection=False)
    await server.close()
    log("disconnected, server closed")


def finish(checks: dict[str, bool], rec: WireRecord) -> int:
    log("summary:")
    for name, ok in checks.items():
        log(f"  {name}: {'ok' if ok else 'missing'}")
    for (run, kind), n in sorted(rec.binary.items()):
        if kind == "audio":
            log(f"  {run}: {n} audio frames sent")
    log(f"  group states sent: {rec.group_states}")
    log(f"  volumes sent: {rec.volumes_sent} mutes sent: {rec.mutes_sent}")
    if rec.blocked_volumes:
        log(f"  volumes blocked (> {MAX_VOLUME}): {rec.blocked_volumes}")
    log(f"  controller supported_commands advertised: {rec.controller_supported}")
    if rec.controller_commands:
        log(f"  controller: commands received {rec.controller_commands}")
    else:
        log("  controller: no command received")
    visual: Counter[str] = Counter()
    for (_run, kind), n in rec.binary.items():
        if kind != "audio":
            visual[kind] += n
    if visual:
        log(f"  visualizer: {sum(visual.values())} frames sent {dict(sorted(visual.items()))}")
    else:
        log("  visualizer: no frames sent")
    ok = all(checks.values())
    log(f"result: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Sendspin test server (aiosendspin): dials a Sendspin client at "
            "ws://<host>:<port>/sendspin, streams a 440 Hz test tone per codec and "
            "exercises volume/mute, group state, controller and visualizer."
        ),
    )
    parser.add_argument("host", help="client host or IP, e.g. 192.168.1.50")
    parser.add_argument("--port", type=int, default=18928, help="client port (default 18928)")
    parser.add_argument(
        "--seconds", type=float, default=10.0, help="tone length per stream (default 10)"
    )
    parser.add_argument(
        "--volume",
        type=int,
        default=15,
        help=f"player volume in percent sent during the first stream, 0-{MAX_VOLUME} "
        "(default 15); the second command uses volume - 5",
    )
    parser.add_argument(
        "--codecs",
        default="flac,pcm",
        help="comma-separated codecs, one stream each, in order (flac, pcm, opus; "
        "default flac,pcm)",
    )
    args = parser.parse_args(argv)
    if not 0 <= args.volume <= MAX_VOLUME:
        parser.error(f"--volume must be between 0 and {MAX_VOLUME} (got {args.volume})")
    if args.seconds <= 0:
        parser.error("--seconds must be positive")
    codecs = [c.strip().lower() for c in args.codecs.split(",") if c.strip()]
    unknown = [c for c in codecs if c not in CODECS]
    if not codecs or unknown:
        parser.error(f"--codecs must list codecs from {sorted(CODECS)} (got {args.codecs!r})")
    args.codecs = codecs
    return args


def main() -> int:
    args = parse_args()
    logging.basicConfig(level=logging.WARNING, format="%(levelname)s %(name)s: %(message)s")
    try:
        return asyncio.run(run(args))
    except KeyboardInterrupt:
        log("interrupted")
        return 130


if __name__ == "__main__":
    sys.exit(main())
