# Max call audio traffic shape

Analyzed from `max.pcap`: a short real Max (VK) call captured at the router via TZSP.
The active media stream runs between `192.0.2.58:41466` ↔ `192.0.2.20:55178` over UDP.
SRTP profile negotiated: `SRTP_AES128_CM_HMAC_SHA1_80` (10-byte auth tag, AES counter mode — no size expansion on payload).

---

## Pacer interval

Silence packets are sent at **20 ms** intervals (±2 ms jitter from the OS scheduler).
This corresponds to the standard Opus 20 ms frame size at 50 packets/second.

```
interval=20.1ms  silence
interval=20.7ms  silence
interval=17.9ms  silence
interval=20.6ms  silence
interval=19.5ms  silence
```

During active speech, multiple data frames may arrive back-to-back within a single 20 ms window (encoder producing several frames at once), but the cadence resets to 20 ms on the next silence/frame boundary.

---

## Packet structure

### Silence frame — 33 bytes UDP payload

Silence frames carry a real **Opus DTX** (discontinuous transmission) silence packet.
The RTP header has the extension bit set (`X=1`) and carries two RFC 5285 one-byte header extensions.

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|V=2|P|X=1|CC=0 |M|   PT=111  |        sequence number         |  <- 4 bytes
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                           timestamp                           |  <- 4 bytes
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                             SSRC                              |  <- 4 bytes
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+  RTP header: 12 bytes
|        0xBEDE (RFC 5285)      |    length = 1 (one word)     |  <- 4 bytes  \
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+               extension: 8 bytes total
| ID=4 | len=0 |  level=0x30   | ID=1 | len=0 |  level=0xFF   |  <- 4 bytes  /
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+  RTP header end: 20 bytes
|           Opus DTX payload (3 bytes, encrypted)               |
|   ...                         +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                               |    HMAC-SHA1-80 (10 bytes)    |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+                               +
|                                                               |
+               +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|               |
+-+-+-+-+-+-+-+-+
```

**Size breakdown:**

| Field | Bytes |
|---|---|
| RTP header (standard) | 12 |
| RTP extension (0xBEDE header + 1 word) | 8 |
| Opus DTX payload (encrypted) | 3 |
| SRTP auth tag (HMAC-SHA1-80) | 10 |
| **Total UDP payload** | **33** |

**RTP header extensions (profile `0xBEDE`, RFC 5285 one-byte format):**

| ID | Mapped URI | Length | Value | Meaning |
|---|---|---|---|---|
| 4 | `urn:ietf:params:rtp-hdrext:sdes:mid` | 1 byte | `0x30` (ASCII '0') | BUNDLE mid — the media stream's mid value is `"0"` |
| 1 | `urn:ietf:params:rtp-hdrext:ssrc-audio-level` | 1 byte | `0xFF` | Audio level: −127 dBov (silence) |

Extension IDs confirmed by SDP exchange extracted from `firefox-calltaker.pcap`:
- Android Chrome offer: `extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level`, `extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid`
- Firefox answer retains only extmap:1 and extmap:4 (drops abs-send-time and transport-cc)

PT=111 is Opus. The marker bit (`M`) is set on the first packet after a silence gap.

### Active speech frame — 293 bytes UDP payload (most common)

During active speech the extension header is absent (`X=0`). The Opus encoder runs in VBR mode; frame sizes vary, but 293-byte UDP payload (271-byte Opus frame) is by far the most common during speech:

```
271 bytes × 8 / 0.020 s ≈ 108 kbps Opus
```

| Field | Bytes |
|---|---|
| RTP header (standard, no extension) | 12 |
| Opus VBR frame (encrypted) | 271 |
| SRTP auth tag (HMAC-SHA1-80) | 10 |
| **Total UDP payload** | **293** |

### Observed UDP payload size distribution (one direction, ~200 packets)

| UDP payload (bytes) | Count | Type |
|---|---|---|
| 33 | 91 | Silence (DTX) |
| 293 | 29 | Speech — dominant frame size |
| 151 | 36 | Speech — smaller frame (lower bitrate moment) |
| 46–230 | ~40 | Speech — VBR tail (various) |
| 646+ | 1 | Burst/retransmit outlier |

---

## Implications for tunnel implementation

To make tunnel traffic resemble a real Max audio call:

| Parameter | Real Max | Implementation |
|---|---|---|
| Pacer interval | **20 ms** | 20 ms (`PACER_INTERVAL`) ✓ |
| Silence UDP payload | **33 bytes** | 33 bytes: 12 (RTP) + 11 (`SILENCE_PAYLOAD_LEN`) + 10 (SRTP auth) ✓ |
| Max data UDP payload | **293 bytes** | 293 bytes: 12 (RTP) + 271 (`MAX_AUDIO_FRAME_PAYLOAD`) + 10 (SRTP auth) ✓ |
| RTP extension on silence | **yes (`0xBEDE`)** | no — Option B chosen (same wire size, simpler) |
| Max tunnel throughput | **~108 kbps** | ~108 kbps ✓ |

Option B was chosen for silence frames: omit the `0xBEDE` extension, set `SILENCE_PAYLOAD_LEN = 11` bytes.
This produces the same 33-byte UDP payload as real Max without the extension header complexity.

`MAX_AUDIO_FRAME_PAYLOAD = 271` caps each data frame at `293 − 12 − 10 = 271` bytes (including the 1-byte tunnel prefix), keeping every packet within the observed dominant audio frame size.

---

## SDP and Opus parameters (from `firefox-calltaker.pcap`)

The WebSocket signaling was decrypted using the exported TLS 1.3 key log.

### Android Chrome offer (PT 63 = RED, PT 111 = Opus)

```
a=rtpmap:111 opus/48000/2
a=fmtp:111 minptime=10;useinbandfec=1;dred=100;usedtx=1
a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level
a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time
a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01
a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid
```

`usedtx=1` in the Opus fmtp line enables DTX — this is why silence frames appear in the capture instead of a continuous stream.

### Firefox answer (PT 111 = Opus only; RED dropped)

```
a=rtpmap:111 opus/48000/2
a=fmtp:111 maxplaybackrate=48000;stereo=1;useinbandfec=1
a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level
a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid
a=setup:active
```

Firefox is DTLS client (`setup:active`). It drops abs-send-time (extmap:2) and transport-cc (extmap:3), keeping only the two extensions used in the wire capture. Firefox does not advertise `usedtx=1`.

### Media stream (direct LAN, no TURN)

`192.0.2.20:60176 ↔ 192.0.2.59:44646` — confirmed SRTP profile `SRTP_AES128_CM_HMAC_SHA1_80`.

---

## RTCP

RTCP Sender Reports are sent every 5 seconds (matching both the Go and Rust implementations).
The SRTCP-encrypted SR packets were not directly visible in this short capture but the interval is consistent with RFC 3550 §6.2 defaults.
