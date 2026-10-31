#!/usr/bin/python3
"""Read-only probe of the Sound Blaster X4 over its CDC serial port.

Unlocks the port (whoareyou/unlock handshake, same scheme as the Katana V2X),
switches to binary mode, then only issues read queries.
"""
import os
import select
import struct
import sys
import tty

from cryptography.hazmat.primitives.ciphers.aead import AESGCM

# Static bytes from Creative's CTCDC.dll (as published in the v2x crate).
KEY_DATA = bytes.fromhex("d31a21279be346f0999d6ec4c3febe98901869c118fbb1256e0ce07b")
PORT = sys.argv[1] if len(sys.argv) > 1 else "/dev/ttyACM0"


def read_until(fd, wait=1.0):
    out = b""
    while select.select([fd], [], [], wait)[0]:
        chunk = os.read(fd, 1024)
        if not chunk:
            break
        out += chunk
        wait = 0.15
    return out


def unlock(fd):
    os.write(fd, b"whoareyou.MyApp8\r\n")
    resp = read_until(fd)
    if resp.startswith(b"whoareyou") and len(resp) >= 45:
        header, pid, nonce = resp[9:11], resp[11:13], resp[13:45]
        key = header + KEY_DATA + pid
        iv = os.urandom(16)
        ct_tag = AESGCM(key).encrypt(iv[:12], nonce, None)
        os.write(fd, b"unlock" + iv + ct_tag + b"\r\n")
        resp = read_until(fd)
        print("unlock ->", resp)
        if not resp.startswith(b"unlock_OK"):
            sys.exit("unlock rejected")
    else:
        print("whoareyou ->", resp, "(assuming already unlocked)")
    os.write(fd, b"SW_MODE1\r\n")
    print("SW_MODE1 ->", read_until(fd, 0.5))


def xfer(fd, payload, wait=0.4):
    os.write(fd, bytes([0x5A]) + bytes(payload))
    return read_until(fd, wait)


def frames(buf):
    """Split a byte stream into 5a CMD LEN PAYLOAD frames."""
    i = 0
    while i + 3 <= len(buf):
        if buf[i] != 0x5A:
            i += 1
            continue
        end = i + 3 + buf[i + 2]
        yield buf[i:end]
        i = end


fd = os.open(PORT, os.O_RDWR | os.O_NOCTTY)
tty.setraw(fd)
read_until(fd, 0.2)  # drain
unlock(fd)

for label, pkt in [
    ("ping", [0x06, 0x01, 0x01]),
    ("identify", [0x05, 0x00]),
    ("firmware", [0x07, 0x01, 0x02]),
    ("firmware (katana)", [0x09, 0x01, 0x02]),
    ("hardware id", [0x20, 0x00]),
    ("global profile", [0x26, 0x03, 0x08, 0xFF, 0xFF]),
    ("output mode", [0x2C, 0x01, 0x01]),
    ("output enum", [0x2C, 0x01, 0x02]),
    ("mic boost", [0x3C, 0x02, 0x01, 0x00]),
    ("range dump", [0x15, 0x01, 0x00]),
]:
    resp = xfer(fd, pkt)
    print(f"{label:18}", " | ".join(f.hex(" ") for f in frames(resp)) or resp or "(no reply)")

print("\nFeature sweep (answers only):")
for fam in (0x95, 0x96, 0x97):
    for fid in range(0x00, 0x48):
        for f in frames(xfer(fd, [0x11, 0x03, 0x01, fam, fid], wait=0.12)):
            if f[1] == 0x11 and len(f) >= 11 and f[5] == fam and f[6] == fid:
                (val,) = struct.unpack("<f", f[7:11])
                print(f"   {fam:02x}:{fid:02x} = {val:g}")
os.close(fd)
