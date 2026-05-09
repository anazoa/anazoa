# Max / OneMe WebSocket Protocol

Documented from decrypted TLS captures, not from injected browser-side WebSocket hooks.

Artifacts used:
- `firefox-calltaker.pcap` + `firefox-calltaker.keys`
- `firefox-caller.pcap` + `firefox-caller.keys`

Important scope note:
- This document is about the OneMe application websocket at `wss://ws-api.oneme.ru/websocket`.
- It is separate from the WebRTC signaling websocket at `wss://videowebrtc.okcdn.ru/ws2`, which is documented in `max-signaling-protocol.md`.
- The JSON files `ws-calltaker.json` and `ws-caller.json` are signaling captures, not OneMe captures.

Where interpretation is uncertain, that is called out explicitly.

---

## Transport

- Protocol: WSS (WebSocket over TLS 1.3)
- Host: `ws-api.oneme.ru`
- Path: `/websocket`
- Message format observed: JSON text frames

Observed streams:
- `firefox-calltaker.pcap`: OneMe websocket is `tcp.stream == 15`
- `firefox-caller.pcap`: OneMe websocket is `tcp.stream == 14`

Unlike the signaling websocket, no WebSocket ping/pong keepalive traffic was observed on these OneMe streams.

---

## Envelope

Observed messages use this envelope:

```json
{
  "ver": 11,
  "cmd": 0,
  "seq": 17,
  "opcode": 1,
  "payload": {"interactive": true}
}
```

Observed fields:
- `ver`: always `11` in these captures
- `cmd`: seen as `0` and `1`
- `seq`: integer sequence number
- `opcode`: integer operation code
- `payload`: JSON object or `null`

Observed `cmd` behavior:
- `cmd: 0` is used for client requests and also for server push notifications
- `cmd: 1` is used for replies / acknowledgements

That means `cmd` is not simply request vs response direction. The closest capture-backed statement is:
- client-initiated requests are sent as `cmd: 0`
- their acknowledgements come back as `cmd: 1`, echoing the same `seq` and `opcode`
- unsolicited server events also arrive as `cmd: 0`

Example request/reply pair from `firefox-calltaker.pcap`:

```json
{"ver":11,"cmd":0,"seq":17,"opcode":1,"payload":{"interactive":true}}
{"ver":11,"cmd":1,"seq":17,"opcode":1,"payload":null}
```

---

## Session start

Both captures start with the same basic OneMe bootstrap.

### 1. Client hello: `opcode 6`

Client sends browser/device metadata:

```json
{
  "ver": 11,
  "cmd": 0,
  "seq": 0,
  "opcode": 6,
  "payload": {
    "userAgent": {
      "deviceType": "WEB",
      "locale": "en",
      "deviceLocale": "en",
      "osVersion": "macOS",
      "deviceName": "Firefox",
      "headerUserAgent": "Mozilla/5.0 ... Firefox/149.0",
      "appVersion": "26.4.1",
      "screen": "1440x3440 1.0x",
      "timezone": "Europe/Moscow"
    },
    "deviceId": "<uuid>"
  }
}
```

Server replies with `opcode 6` and `cmd: 1`:

```json
{
  "ver": 11,
  "cmd": 1,
  "seq": 0,
  "opcode": 6,
  "payload": {
    "phone-auth-enabled": false,
    "reg-country-code": ["AZ", "..."],
    "location": "RU"
  }
}
```

### 2. Chat/session sync: `opcode 19`

Client then sends:

```json
{
  "ver": 11,
  "cmd": 0,
  "seq": 1,
  "opcode": 19,
  "payload": {
    "token": "<auth token>",
    "chatsCount": 40,
    "interactive": true,
    "chatsSync": 0,
    "contactsSync": 0,
    "presenceSync": -1,
    "draftsSync": 0
  }
}
```

The reply to `opcode 19` is large and appears to bootstrap application state:
- profile/contact data
- chat list
- config blob
- contacts

This looks like the main authenticated session restore / sync request.

Notable observation:
- `interactive` is already `true` in this initial sync request, not only in later heartbeat messages.

---

## Observed client-driven opcodes

These opcodes were seen sent by the browser client on the OneMe websocket.

| Opcode | Observed payload shape | Likely purpose |
|---|---|---|
| `1` | `{"interactive": true}` | App-level heartbeat / activity signal |
| `5` | `{"events":[...]}` | Analytics / navigation / call telemetry upload |
| `6` | `{"userAgent":...,"deviceId":...}` | Initial client hello |
| `19` | `{"token":...,"interactive":true,...}` | Authenticated chat/session sync |
| `27` | `{"type":"STICKER"|"FAVORITE_STICKER"|"REACTION"|"ANIMOJI_SET","sync":0}` | Asset sync |
| `32` | `{"contactIds":[...]}` | Fetch contacts by id |
| `35` | `{"contactIds":[...]}` | Fetch presence by user id |
| `48` | `{"chatIds":[...]}` | Fetch chats by id |
| `75` | `{"chatId":00000000,"subscribe":true|false}` | Subscribe / unsubscribe chat updates |
| `79` | `{"forward":false,"count":100}` | Fetch recent history |
| `180` | `{"chatId":...,"messageIds":[...]}` | Fetch reactions for messages |
| `272` | `{"folderSync":0}` | Folder sync |

These meanings are inferred from payload shape and paired responses, not from server source.

---

## Observed server-push opcodes

These opcodes were seen arriving from the server as unsolicited `cmd: 0` messages.

| Opcode | Observed payload shape | Likely purpose |
|---|---|---|
| `128` | chat/message object, unread count | New message / chat delta |
| `132` | `{"presence":{"seen":...},"userId":...}` | Presence update |
| `137` | `{"vcp":...,"callerId":...,"conversationId":...,"type":"AUDIO"}` | Incoming call notification |
| `292` | banners blob | Banner/config update |

Again, the names are inferred from payload contents.

---

## Incoming call notification

The most important call-related OneMe push is `opcode 137`.

Observed in `firefox-calltaker.pcap`, frame `1013`:

```json
{
  "ver": 11,
  "cmd": 0,
  "seq": 2,
  "opcode": 137,
  "payload": {
    "vcp": "<opaque blob>",
    "callerId": <caller_user_id>,
    "chatId": 0,
    "type": "AUDIO",
    "conversationId": "<conversation_id>"
  }
}
```

Capture-backed facts:
- `callerId` is the numeric OneMe user id of the caller
- `conversationId` matches the call id later used on the signaling websocket
- `type` was `AUDIO` in the observed call
- `vcp` contains enough information to bootstrap signaling and TURN

From local decoding work in `rtc-tun`, the `vcp` blob contains at least:
- signaling token
- signaling websocket URL
- STUN server
- TURN URLs
- TURN username
- TURN password

The exact `vcp` encoding is not documented here; only the fact that it carries call bootstrap data is capture-backed.

---

## Heartbeat / liveness behavior

This is the main operational finding from the captures.

Observed behavior:
- The OneMe websocket does not show WebSocket ping/pong keepalives.
- Instead, the client periodically sends `opcode 1` with `{"interactive": true}`.
- The server acknowledges with the same `seq` and `opcode`, `cmd: 1`, `payload: null`.

Examples:

Calltaker capture:
- frame `791`: client sends `opcode 1`
- frame `2476`: client sends `opcode 1` during the call
- frame `2481`: server acknowledges it

Caller capture:
- frame `694`: initial `opcode 1`
- frame `2567`: `opcode 1` during the call
- frame `2571`: ack
- frame `2858`: `opcode 1` after the call
- frame `2861`: ack

Interpretation:
- `opcode 1` is the OneMe application heartbeat / activity signal
- It continues during calls and may continue after the call ends until the session is closed

This is why `rtc-tun` now uses app-level interactive heartbeats for OneMe rather than WS ping/pong.

---

## Call relationship

The captures show two separate websockets during a call:

1. OneMe websocket: `ws-api.oneme.ru/websocket`
   - login/bootstrap
   - chat/presence updates
   - incoming call push (`opcode 137`)
   - app-level heartbeats (`opcode 1`)
   - analytics upload (`opcode 5`)

2. Signaling websocket: `videowebrtc.okcdn.ru/ws2`
   - SDP / ICE exchange
   - signaling notifications
   - plain-text `ping` / `pong`

Observed sequencing in the calltaker capture:
- OneMe `opcode 137` incoming call arrives first
- then the client opens the signaling websocket and handles call setup there
- after the call ends, the OneMe websocket remains alive long enough to receive chat and presence updates

Observed sequencing in the caller capture:
- no outgoing-call initiation was observed on the OneMe websocket
- the call setup happens through other paths, while the OneMe websocket continues to carry chat/session traffic

This supports the current `rtc-tun` model:
- OneMe is for session state and incoming-call notification
- signaling websocket is for call setup and teardown

---

## Post-call behavior

Both captures show the OneMe websocket staying open briefly after hangup.

Observed after-call traffic:
- chat delta (`opcode 128`) carrying the call message / call attach
- presence update (`opcode 132`)
- analytics upload (`opcode 5`)

Examples:
- Calltaker: frames `3048`, `3260`, `3293`
- Caller: frames `2696`, `2839`, `2847`, `2858`, `2868`

Only after that post-call traffic does the websocket close.

The close looked clean in both captures:
- `firefox-calltaker.pcap`: close around frame `3295`
- `firefox-caller.pcap`: close around frame `2871`

No capture-backed evidence here suggests that the real browser client intentionally drops the OneMe socket during a healthy call.

---

## Analytics / telemetry (`opcode 5`)

`opcode 5` is consistently used for analytics uploads.

Observed payload shape:

```json
{
  "events": [
    {
      "type": "CALL" | "NAV" | "CONTACT",
      "userId": <user_id>,
      "sessionId": 1700000000000,
      "time": 1700000000000,
      "event": "START_CALL" | "FINISH_CALL" | "GO" | "...",
      "params": {...}
    }
  ]
}
```

Observed event kinds include:
- `CALL`
- `NAV`
- `CONTACT`

Observed call-related event names include:
- `INCOMING_CALL_INIT`
- `INCOMING_CALL_RECEIVED`
- `START_CALL`
- `FINISH_CALL`

The server acknowledges `opcode 5` with `payload: null`.

This looks useful for reverse-engineering call lifecycle timing, but `rtc-tun` does not currently need it for functionality.

---

## Gaps / unknowns

What the captures do not establish reliably:
- Full opcode catalog
- Exact semantics of `cmd`
- Whether `seq` is only client-generated or meaningful across both directions
- Whether the heartbeat interval is fixed by protocol or just by this browser implementation
- Whether some OneMe operations can arrive in binary frames in other clients
- Exact `vcp` encoding format and versioning rules

What we can say confidently from these captures:
- OneMe uses a JSON websocket protocol with `ver/cmd/seq/opcode/payload`
- `opcode 137` delivers incoming call bootstrap data
- `opcode 1` with `{"interactive": true}` is the observed keepalive/activity mechanism
- OneMe and WebRTC signaling are separate websockets with different liveness behavior

---

## Reproducing the inspection

Example `tshark` commands used:

```bash
tshark -r reveng/firefox-calltaker.pcap \
  -o tls.keylog_file:reveng/firefox-calltaker.keys \
  -Y 'http.request.uri contains "/websocket" || websocket' \
  -T fields \
  -e frame.number -e tcp.stream -e ip.dst -e http.host -e http.request.uri \
  -e websocket.payload.text
```

```bash
tshark -r reveng/firefox-caller.pcap \
  -o tls.keylog_file:reveng/firefox-caller.keys \
  -Y 'http.request.uri contains "/websocket" || websocket' \
  -T fields \
  -e frame.number -e tcp.stream -e ip.dst -e http.host -e http.request.uri \
  -e websocket.payload.text
```
