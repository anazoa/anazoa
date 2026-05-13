#!/usr/bin/env python3
"""
Callback order tool.

Usage:
  callback.py [-c config.toml] order     -- send order, arm local answer window
  callback.py [-c config.toml] listen    -- wait for orders via IMAP IDLE, place call
  callback.py [-c config.toml] test-rtt  -- measure SMTP→IMAP delivery latency

Configuration file (default: callback.toml next to this script):
  address-to                      -- address to send orders to
  address-from                    -- address to expect orders from
  subject                         -- email subject line
  body                            -- email body (also matched by the listener)

  [smtp]
    host, user, password          -- outgoing mail (order)
    port                          -- default 465

  [imap]
    host, user, password          -- incoming mail (listen)
    port                          -- default 993

  [ctl]
    executable                    -- default "anazoa-ctl"
    socket                        -- default "/run/anazoa.sock"
    window_secs                   -- answer window duration, default 60

Both sides use the same config.
"""

import email as _email
import imaplib
import os
import smtplib
import socket
import subprocess
import sys
import time
from email.message import EmailMessage

import tomllib

_SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))


def load_config(path: str) -> dict:
    with open(path, "rb") as f:
        return tomllib.load(f)


def _smtp_send(cfg: dict) -> None:
    smtp = cfg["smtp"]
    msg = EmailMessage()
    msg["Subject"] = cfg["subject"]
    msg["From"] = smtp["user"]
    msg["To"] = cfg["address-to"]
    msg.set_content(cfg["body"] + "\n")
    with smtplib.SMTP_SSL(smtp["host"], smtp.get("port", 465)) as s:
        s.login(smtp["user"], smtp["password"])
        s.send_message(msg)


def cmd_order(cfg: dict) -> None:
    ctl = cfg.get("ctl", {})
    _smtp_send(cfg)
    print("Order sent", flush=True)
    window_secs = ctl.get("window_secs", 60)
    subprocess.run(
        [
            ctl.get("executable", "anazoa-ctl"),
            "-s",
            ctl.get("socket", "/run/anazoa.sock"),
            "answer",
            str(window_secs),
        ],
        check=False,
    )
    print(f"Answer window open ({window_secs}s)", flush=True)


def _imap_connect(cfg: dict) -> imaplib.IMAP4_SSL:
    imap_cfg = cfg["imap"]
    imap = imaplib.IMAP4_SSL(imap_cfg["host"], imap_cfg.get("port", 993))
    imap.login(imap_cfg["user"], imap_cfg["password"])
    imap.select("INBOX")
    return imap


def _decode_body(raw: bytes) -> str:
    msg = _email.message_from_bytes(raw)
    if msg.is_multipart():
        for part in msg.walk():
            if part.get_content_type() == "text/plain":
                payload = part.get_payload(decode=True)
                return (
                    payload.decode(errors="replace")
                    if isinstance(payload, bytes)
                    else ""
                )
        return ""
    payload = msg.get_payload(decode=True)
    return payload.decode(errors="replace") if isinstance(payload, bytes) else ""


def _find_order(imap: imaplib.IMAP4_SSL, cfg: dict, from_addr: str) -> bool:
    """Search for an unseen order message; fetch (marks seen) and return True if found."""
    _, data = imap.search(None, "UNSEEN", f'FROM "{from_addr}"')
    for seq in data[0].split() if data[0] else []:
        _, msg_data = imap.fetch(seq, "(BODY[])")
        if not msg_data or not isinstance(msg_data[0], tuple):
            continue
        if cfg["body"] in _decode_body(msg_data[0][1]):
            return True
    return False


def _process_unseen(imap: imaplib.IMAP4_SSL, cfg: dict) -> None:
    if not _find_order(imap, cfg, cfg["address-from"]):
        return
    ctl = cfg.get("ctl", {})
    print("Order received, calling", flush=True)
    subprocess.run(
        [
            ctl.get("executable", "anazoa-ctl"),
            "-s",
            ctl.get("socket", "/run/anazoa.sock"),
            "call",
        ],
        check=False,
    )
    print("Call placed", flush=True)


def _idle_wait(imap: imaplib.IMAP4_SSL, timeout: float = 28 * 60) -> None:
    """Block until the server signals new mail or timeout seconds elapse."""
    tag = imap._new_tag()
    imap.send(tag + b" IDLE\r\n")

    line = imap.readline()
    if not line.startswith(b"+"):
        del imap.tagged_commands[tag]
        return

    imap.socket().settimeout(timeout)
    try:
        imap.readline()
    except (socket.timeout, TimeoutError, OSError):
        pass
    finally:
        imap.socket().settimeout(None)
        imap.send(b"DONE\r\n")
        while True:
            line = imap.readline()
            if line.startswith(tag):
                break
    del imap.tagged_commands[tag]


def cmd_test_rtt(cfg: dict) -> None:
    imap = _imap_connect(cfg)

    t0 = time.monotonic()
    _smtp_send(cfg)
    print("Probe sent, waiting for delivery...", flush=True)

    deadline = t0 + 5 * 60
    while True:
        if _find_order(imap, cfg, cfg["address-from"]):
            print(f"RTT: {time.monotonic() - t0:.2f}s", flush=True)
            return
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            print("Timeout: no delivery within 5 minutes", file=sys.stderr, flush=True)
            sys.exit(1)
        _idle_wait(imap, remaining)


def cmd_listen(cfg: dict) -> None:
    print(f"Listening for orders from {cfg['address-from']} via IMAP IDLE", flush=True)
    while True:
        try:
            imap = _imap_connect(cfg)
            # Handle messages that arrived before we connected.
            _process_unseen(imap, cfg)
            while True:
                _idle_wait(imap)
                _process_unseen(imap, cfg)
        except (imaplib.IMAP4.error, OSError, socket.error) as e:
            print(f"IMAP error: {e}, reconnecting in 10s", flush=True)
            time.sleep(10)


def main() -> None:
    args = sys.argv[1:]
    config_path = os.path.join(_SCRIPT_DIR, "callback.toml")

    if len(args) >= 2 and args[0] == "-c":
        config_path = args[1]
        args = args[2:]

    commands = {"order": cmd_order, "listen": cmd_listen, "test-rtt": cmd_test_rtt}
    if len(args) != 1 or args[0] not in commands:
        print(
            f"Usage: {sys.argv[0]} [-c config.toml] order|listen|test-rtt",
            file=sys.stderr,
        )
        sys.exit(1)

    cfg = load_config(config_path)

    commands[args[0]](cfg)


if __name__ == "__main__":
    main()
