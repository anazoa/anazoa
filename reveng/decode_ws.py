#!/usr/bin/env python3
import json
import re
import subprocess
import sys
import zlib

# --- configure these ---
pcap = "/tmp/capture.pcap"
keys = "/tmp/max.keys"
stream = 3  # TCP stream number (tshark -Y tls -T fields -e tcp.stream | sort -nu)
# -----------------------

result = subprocess.run(
    [
        "tshark",
        "-r",
        pcap,
        "-o",
        f"tls.keylog_file:{keys}",
        "-q",
        "-z",
        f"follow,tls,hex,{stream}",
    ],
    capture_output=True,
    text=True,
)

lines = result.stdout.splitlines()

node0 = node1 = ""
for line in lines:
    m = re.match(r"Node 0: (.+)", line)
    if m:
        node0 = m.group(1)
    m = re.match(r"Node 1: (.+)", line)
    if m:
        node1 = m.group(1)

client_is_node0 = "192.168" in node0
byte_re = re.compile(r"\b[0-9a-f]{2}\b", re.I)

client_buf = bytearray()
server_buf = bytearray()

for line in lines:
    # Hex dump lines: optional tab, 8-digit offset, two spaces, then hex bytes and ASCII
    # Strip leading/trailing whitespace for direction detection, but preserve leading tab
    is_node1 = line.startswith("\t")
    stripped = line.strip()
    # Must start with 8 hex digits (offset)
    if not re.match(r"^[0-9a-f]{8}\s", stripped, re.I):
        continue
    # Extract the hex portion: it's between position 10 and the ASCII column (col 60 of stripped line)
    # Format: "XXXXXXXX  HH HH HH HH HH HH HH HH  HH HH HH HH HH HH HH HH  AAAAAAAAAAAAAAAA"
    # hex section = chars 10 to 58 of stripped
    hex_section = stripped[10:58]
    data = bytes(int(b, 16) for b in byte_re.findall(hex_section))
    is_client = (not is_node1) if client_is_node0 else is_node1
    if is_client:
        client_buf.extend(data)
    else:
        server_buf.extend(data)

print(
    f"Client total: {len(client_buf)}, Server total: {len(server_buf)}", file=sys.stderr
)


def skip_http(buf):
    idx = bytes(buf).find(b"\r\n\r\n")
    if idx >= 0:
        print(f"  Found HTTP end at {idx}, ws starts at {idx + 4}", file=sys.stderr)
        return buf[idx + 4 :]
    return buf


def parse_and_decode(ws_bytes, label):
    dec = zlib.decompressobj(wbits=-15)
    i = 0
    n = 0
    while i + 2 <= len(ws_bytes):
        b0, b1 = ws_bytes[i], ws_bytes[i + 1]
        rsv1 = (b0 >> 6) & 1
        opcode = b0 & 0x0F
        masked = (b1 >> 7) & 1
        plen = b1 & 0x7F
        i += 2
        if plen == 126:
            if i + 2 > len(ws_bytes):
                break
            plen = int.from_bytes(ws_bytes[i : i + 2], "big")
            i += 2
        elif plen == 127:
            if i + 8 > len(ws_bytes):
                break
            plen = int.from_bytes(ws_bytes[i : i + 8], "big")
            i += 8
        mask_key = b""
        if masked:
            if i + 4 > len(ws_bytes):
                break
            mask_key = ws_bytes[i : i + 4]
            i += 4
        if i + plen > len(ws_bytes):
            break
        payload = bytearray(ws_bytes[i : i + plen])
        i += plen
        if masked:
            for j in range(len(payload)):
                payload[j] ^= mask_key[j % 4]
        n += 1
        if opcode == 8:
            print(f"\n[{label} #{n}] CLOSE")
            continue
        if opcode == 9:
            print(f"\n[{label} #{n}] PING")
            continue
        if opcode == 10:
            print(f"\n[{label} #{n}] PONG")
            continue
        if opcode not in (1, 2):
            continue
        if rsv1:
            try:
                payload = dec.decompress(bytes(payload) + b"\x00\x00\xff\xff")
            except Exception as e:
                print(f"\n[{label} #{n}] decompress err: {e}")
                continue
        try:
            text = bytes(payload).decode("utf-8", errors="replace")
            try:
                text = json.dumps(json.loads(text), ensure_ascii=False, indent=2)
            except Exception:
                pass
            print(f"\n[{label} #{n}]")
            print(text)
        except Exception as e:
            print(f"\n[{label} #{n}] err: {e}")


client_ws = bytes(skip_http(client_buf))
server_ws = bytes(skip_http(server_buf))
print(f"Client WS: {len(client_ws)}, first: {client_ws[:4].hex()}", file=sys.stderr)
print(f"Server WS: {len(server_ws)}, first: {server_ws[:4].hex()}", file=sys.stderr)

parse_and_decode(client_ws, "CLIENT→SERVER")
parse_and_decode(server_ws, "SERVER→CLIENT")
