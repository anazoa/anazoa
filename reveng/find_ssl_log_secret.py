#!/usr/bin/env python3
"""
find_ssl_log_secret.py — locate ssl_log_secret in a stripped arm64 BoringSSL SO.

Scans for ADRP+ADD pairs that reference one of the known SSLKEYLOGFILE label
strings ("CLIENT_HANDSHAKE_TRAFFIC_SECRET", etc.).  All such pairs should be
in small wrapper functions that tail-call one common function: ssl_log_secret.
Prints the offset of that common callee.

Usage:
    python3 find_ssl_log_secret.py libjingle-arm64.so
"""

import sys
import struct

LABELS = [
    b"CLIENT_HANDSHAKE_TRAFFIC_SECRET",
    b"SERVER_HANDSHAKE_TRAFFIC_SECRET",
    b"CLIENT_TRAFFIC_SECRET_0",
    b"SERVER_TRAFFIC_SECRET_0",
    b"EXPORTER_SECRET",
    b"CLIENT_RANDOM",
    b"EARLY_EXPORTER_SECRET",
]


def adrp_imm(insn, pc):
    """Return the page-aligned address produced by an ADRP instruction."""
    immhi = (insn >> 5) & 0x7FFFF
    immlo = (insn >> 29) & 0x3
    imm = ((immhi << 2) | immlo) << 12
    # sign-extend from bit 32
    if imm & (1 << 32):
        imm -= 1 << 33
    return (pc & ~0xFFF) + imm


def add_imm12(insn):
    """Return the imm12 field of an ADD (immediate) instruction."""
    return (insn >> 10) & 0xFFF


def scan_file(path):
    with open(path, "rb") as f:
        data = f.read()

    # Build map: string VA → label text
    label_addrs = {}
    for label in LABELS:
        off = 0
        while True:
            idx = data.find(label, off)
            if idx == -1:
                break
            # must be null-terminated
            if idx + len(label) < len(data) and data[idx + len(label)] == 0:
                label_addrs[idx] = label.decode()
            off = idx + 1

    if not label_addrs:
        print("No SSLKEYLOGFILE label strings found — wrong binary?")
        return

    print(f"Found {len(label_addrs)} label string(s):")
    for va, name in sorted(label_addrs.items()):
        print(f"  0x{va:08x}  {name}")

    # Scan .text for ADRP+ADD pairs that load a label address
    # ADRP: bits[31:24] = 0x90 (but bit 31=1, bits[28:24]=10000)
    # Mask: 0x9F000000, value: 0x90000000
    # ADD imm: bits[31:24] = 0x91
    callees = []
    n = len(data) // 4
    for i in range(n - 4):
        off = i * 4
        insn = struct.unpack_from("<I", data, off)[0]
        if (insn & 0x9F000000) != 0x90000000:
            continue
        # This is an ADRP
        page = adrp_imm(insn, off)
        rd = insn & 0x1F

        add_off = off + 4
        add_insn = struct.unpack_from("<I", data, add_off)[0]
        # ADD (immediate): opcode 0x91, same Rd/Rn
        if (add_insn >> 24) & 0xFF != 0x91:
            continue
        rn = (add_insn >> 5) & 0x1F
        add_rd = add_insn & 0x1F
        if rn != rd:
            continue

        imm12 = add_imm12(add_insn)
        target_va = page + imm12

        if target_va not in label_addrs:
            continue

        label_name = label_addrs[target_va]
        print(f"\n  ADRP+ADD for '{label_name}' at 0x{off:08x}")

        # Look for a BL or B (tail-call) within the next ~16 instructions
        for j in range(1, 24):
            scan_off = add_off + j * 4
            if scan_off + 4 > len(data):
                break
            branch = struct.unpack_from("<I", data, scan_off)[0]
            # BL: 0x94000000 mask 0xFC000000
            if (branch & 0xFC000000) == 0x94000000:
                imm26 = branch & 0x03FFFFFF
                if imm26 & (1 << 25):
                    imm26 -= 1 << 26
                dest = scan_off + imm26 * 4
                print(f"    BL  at 0x{scan_off:08x} → 0x{dest:08x}")
                callees.append(dest)
                break
            # B (unconditional): 0x14000000 mask 0xFC000000
            if (branch & 0xFC000000) == 0x14000000:
                imm26 = branch & 0x03FFFFFF
                if imm26 & (1 << 25):
                    imm26 -= 1 << 26
                dest = scan_off + imm26 * 4
                print(f"    B   at 0x{scan_off:08x} → 0x{dest:08x}")
                callees.append(dest)
                break

    if not callees:
        print("\nNo BL/B found near label references.")
        return

    from collections import Counter
    freq = Counter(callees)
    best, count = freq.most_common(1)[0]
    print(f"\nMost common callee: 0x{best:08x}  (referenced {count}×)")
    print(f"\n→ ssl_log_secret offset: 0x{best:08x}")
    print(f"  (Update SSL_LOG_SECRET_OFFSET in tls-keylog.js if different)")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <libjingle-arm64.so>")
        sys.exit(1)
    scan_file(sys.argv[1])
