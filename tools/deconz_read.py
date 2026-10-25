#!/usr/bin/env python3
"""Read-only deCONZ client for pulsar's coordinator (docs/stations.md, "Радио Zigbee").

Raw SLIP over TCP, only these commands: 0x0D version, 0x07 device state, 0x0A read parameter
(MAC address, PAN ID, extended PAN ID, current channel, NWK update id, protocol version). It never
writes a parameter or changes the network state. Output: one "name=value" line each, stable for
diff between two builds (e.g. a test instance and zigbee_probe).

Connecting replaces the server's current deCONZ host (Zigbee2MQTT / ZHA get disconnected): use it
only against the test instance (16638) or a zigbee_probe port with the service stopped, never
against the live daemon's 6638.

usage: deconz_read.py --host <ip> --port <port>
"""

import argparse
import socket
import struct
import sys

END, ESC, ESC_END, ESC_ESC = 0xC0, 0xDB, 0xDC, 0xDD
VERSION, DEVICE_STATE, READ_PARAMETER = 0x0D, 0x07, 0x0A
DEVICE_STATE_CHANGED = 0x0E
PARAMS = [
    ("mac", 0x01, "<Q", "{:016x}"),
    ("pan_id", 0x05, "<H", "0x{:04x}"),
    ("ext_pan_id", 0x08, "<Q", "{:016x}"),
    ("channel", 0x1C, "<B", "{}"),
    ("nwk_update_id", 0x24, "<B", "{}"),
    ("protocol_version", 0x22, "<H", "0x{:04x}"),
]


def checksum(frame: bytes) -> int:
    return (~sum(frame) + 1) & 0xFFFF


def encode(cmd: int, seq: int, payload: bytes) -> bytes:
    frame = bytes([cmd, seq, 0]) + struct.pack("<H", 5 + len(payload)) + payload
    frame += struct.pack("<H", checksum(frame))
    out = bytearray([END])
    for b in frame:
        if b == END:
            out += bytes([ESC, ESC_END])
        elif b == ESC:
            out += bytes([ESC, ESC_ESC])
        else:
            out.append(b)
    out.append(END)
    return bytes(out)


class Client:
    def __init__(self, host: str, port: int):
        self.sock = socket.create_connection((host, port), timeout=5)
        self.buf = bytearray()
        self.seq = 0

    def frames(self):
        while True:
            while END in self.buf:
                i = self.buf.index(END)
                raw, self.buf = bytes(self.buf[:i]), self.buf[i + 1 :]
                if not raw:
                    continue
                f = bytearray()
                it = iter(raw)
                for b in it:
                    if b == ESC:
                        b = next(it, b)
                        b = END if b == ESC_END else ESC if b == ESC_ESC else b
                    f.append(b)
                if len(f) < 7 or checksum(f[:-2]) != struct.unpack("<H", f[-2:])[0]:
                    continue
                yield bytes(f[:-2])
            data = self.sock.recv(4096)
            if not data:
                raise ConnectionError("connection closed (radio not ready?)")
            self.buf += data

    def request(self, cmd: int, payload: bytes) -> tuple[int, bytes]:
        self.seq = self.seq % 255 + 1
        self.sock.sendall(encode(cmd, self.seq, payload))
        for f in self.frames():
            if f[0] == DEVICE_STATE_CHANGED:
                continue
            if f[0] == cmd and f[1] == self.seq:
                return f[2], f[5:]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--host", required=True)
    ap.add_argument("--port", type=int, required=True)
    args = ap.parse_args()
    c = Client(args.host, args.port)

    status, body = c.request(VERSION, b"\0\0\0\0")
    print(f"version=0x{struct.unpack('<I', body[:4])[0]:08x}" if status == 0 else f"version=status {status}")
    status, body = c.request(DEVICE_STATE, b"\0\0\0")
    if status == 0:
        print(f"device_state=0x{body[0]:02x} network_state={body[0] & 3}")
    else:
        print(f"device_state=status {status}")
    for name, pid, fmt, show in PARAMS:
        status, body = c.request(READ_PARAMETER, struct.pack("<HB", 1, pid))
        # [payload length][id][value]
        size = struct.calcsize(fmt)
        if status != 0 or len(body) < 3 + size or body[2] != pid:
            print(f"{name}=status {status}")
            continue
        print(f"{name}=" + show.format(struct.unpack(fmt, body[3 : 3 + size])[0]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
