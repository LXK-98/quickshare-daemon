#!/usr/bin/env python3
"""Convert Android's btsnooz blob (from `dumpsys bluetooth_manager`) to btsnoop.

Format per AOSP system/tools/scripts/btsnooz.py:
  base64 { version:i8, last_timestamp_ms:u64, deflate { records } }
  v2 record: length:u16, packet_length:u16, delta_ms:u32, type:i8, payload
"""
import base64, struct, sys, zlib

# btsnooz packet types -> (h4 type byte, is_received)
TYPES = {
    0x10: (0x04, True), 0x11: (0x02, True), 0x12: (0x03, True), 0x17: (0x05, True),
    0x20: (0x01, False), 0x21: (0x02, False), 0x22: (0x03, False), 0x27: (0x05, False),
}

def convert(b64_text, out_path):
    snooz = base64.b64decode("".join(b64_text.split()))
    version, last_timestamp_ms = struct.unpack_from("=bQ", snooz, 0)
    if version not in (1, 2):
        raise SystemExit(f"unsupported btsnooz version {version}")
    data = zlib.decompress(snooz[9:])

    hdr_fmt, hdr_len = ("=HHIb", 9) if version == 2 else ("=HIb", 7)

    # Pass 1: recover the first timestamp by subtracting every delta.
    first = last_timestamp_ms + 0x00dcddb30f2f8000
    off = 0
    records = []
    while off + hdr_len <= len(data):
        fields = struct.unpack_from(hdr_fmt, data, off)
        if version == 2:
            length, packet_length, delta, stype = fields
        else:
            length, delta, stype = fields
            packet_length = length
        payload = data[off + hdr_len: off + hdr_len + length - 1]
        records.append((packet_length, length, delta, stype, payload))
        first -= delta
        off += hdr_len + length - 1

    with open(out_path, "wb") as out:
        out.write(b"btsnoop\x00" + struct.pack(">II", 1, 1002))
        ts = first
        kept = skipped = 0
        for packet_length, length, delta, stype, payload in records:
            ts += delta
            if stype not in TYPES:
                skipped += 1
                continue
            h4, received = TYPES[stype]
            out.write(struct.pack(">II", packet_length, length))
            out.write(struct.pack(">II", 1 if received else 0, 0))
            out.write(struct.pack(">II", ts >> 32, ts & 0xFFFFFFFF))
            out.write(bytes([h4]) + payload)
            kept += 1
    print(f"version {version}: {kept} records written, {skipped} of unknown type")

if __name__ == "__main__":
    convert(open(sys.argv[1]).read(), sys.argv[2])
