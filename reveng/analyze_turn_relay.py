#!/usr/bin/env python3

import argparse
import collections
import re
import subprocess
import sys


STUN_MAGIC_COOKIE = bytes.fromhex("2112a442")

STUN_TYPES = {
    0x0001: "binding_req",
    0x0101: "binding_ok",
    0x0003: "allocate_req",
    0x0103: "allocate_ok",
    0x0004: "refresh_req",
    0x0104: "refresh_ok",
    0x0008: "create_permission_req",
    0x0108: "create_permission_ok",
    0x0009: "channel_bind_req",
    0x0109: "channel_bind_ok",
    0x0016: "send_indication",
    0x0113: "data_indication",
}


def tshark_rows(path: str, display_filter: str) -> list[list[str]]:
    cmd = [
        "tshark",
        "-r",
        path,
        "-Y",
        display_filter,
        "-T",
        "fields",
        "-E",
        "separator=\t",
        "-e",
        "frame.number",
        "-e",
        "frame.time_relative",
        "-e",
        "ip.src",
        "-e",
        "udp.srcport",
        "-e",
        "ip.dst",
        "-e",
        "udp.dstport",
        "-e",
        "frame.len",
        "-e",
        "udp.payload",
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True, check=True)
    rows: list[list[str]] = []
    for line in proc.stdout.splitlines():
        if not line.strip():
            continue
        parts = line.split("\t")
        if len(parts) >= 8:
            rows.append(parts[:8])
    return rows


def clean_hex(value: str) -> str:
    return re.sub(r"[^0-9A-Fa-f]", "", value)


def is_dtls(buf: bytes) -> bool:
    return len(buf) >= 3 and buf[0] in (20, 21, 22, 23) and buf[1] == 0xFE and buf[2] in (0xFD, 0xFF)


def is_stun(buf: bytes) -> bool:
    return len(buf) >= 20 and (buf[0] & 0xC0) == 0 and buf[4:8] == STUN_MAGIC_COOKIE


def parse_stun(buf: bytes):
    if not is_stun(buf):
        return None

    msg_type = (buf[0] << 8) | buf[1]
    msg_len = (buf[2] << 8) | buf[3]
    pos = 20
    end = min(len(buf), 20 + msg_len)
    attrs = []

    while pos + 4 <= end:
        attr_type = (buf[pos] << 8) | buf[pos + 1]
        attr_len = (buf[pos + 2] << 8) | buf[pos + 3]
        attr_val = buf[pos + 4 : pos + 4 + attr_len]
        attrs.append((attr_type, attr_val))
        pos += 4 + ((attr_len + 3) & ~3)

    return msg_type, attrs


def classify_inner(inner: bytes) -> str:
    if not inner:
        return "empty"
    if is_stun(inner):
        return "stun"
    if is_dtls(inner):
        return "dtls"
    if inner[0] in (0x80, 0x81, 0x90, 0x91):
        return "media"
    return "media"


def classify_packet(buf: bytes) -> str:
    if not buf:
        return "empty"

    if len(buf) >= 4 and (buf[0] & 0xC0) == 0x40:
        inner_len = (buf[2] << 8) | buf[3]
        inner = buf[4 : 4 + inner_len]
        return f"channel_{classify_inner(inner)}"

    parsed = parse_stun(buf)
    if parsed:
        msg_type, attrs = parsed
        data_attr = None
        for attr_type, attr_val in attrs:
            if attr_type == 0x0013:
                data_attr = attr_val
                break
        if data_attr is not None:
            return f"sendind_{classify_inner(data_attr)}"
        return STUN_TYPES.get(msg_type, f"stun_{msg_type:04x}")

    if is_dtls(buf):
        return "dtls"

    return "other"


def summarize(rows: list[list[str]]) -> None:
    counts = collections.Counter()
    sizes = collections.defaultdict(collections.Counter)
    samples = collections.defaultdict(list)

    for frame_no, rel_time, src, sport, dst, dport, frame_len, payload in rows:
        payload_hex = clean_hex(payload)
        buf = bytes.fromhex(payload_hex) if payload_hex else b""
        label = classify_packet(buf)
        counts[label] += 1
        sizes[label][int(frame_len)] += 1
        if len(samples[label]) < 6:
            samples[label].append((frame_no, rel_time, src, sport, dst, dport, frame_len))

    for label, count in counts.most_common():
        print(f"{label}: {count}")
        print(f"  sizes: {sizes[label].most_common(6)}")
        print("  sample:")
        for sample in samples[label]:
            print("   ", sample)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Classify TURN relay UDP packets into STUN, DTLS, and likely media buckets."
    )
    parser.add_argument("pcap", help="pcap file to inspect")
    parser.add_argument(
        "--host",
        help="limit packets to frames where ip.addr matches this IPv4 address",
    )
    parser.add_argument(
        "--filter",
        default="udp",
        help="extra tshark display filter, appended with 'and'",
    )
    args = parser.parse_args()

    display_filter = args.filter
    if args.host:
        display_filter = f"({display_filter}) and ip.addr=={args.host}"

    try:
        rows = tshark_rows(args.pcap, display_filter)
    except subprocess.CalledProcessError as exc:
        sys.stderr.write(exc.stderr)
        return exc.returncode

    summarize(rows)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
