# Max Linux Client — Full Signaling Protocol

Captured from Max Linux 26.12.0 (libcall-service.so) against Firefox/OneMe callee.
Both calltaker and caller pcaps decoded, all TLS connections captured.

---

## REST API (`calls.okcdn.ru`)

Host resolves to 155.212.204.x (no SNI in ClientHello — resolved differently from Qt connections).
`User-Agent: MAX/1.44.54144 (X11; linux; Linux 13) Boost.Beast/357`
`application_key=CHFNKILGDIHBABABA`

### 1. `GET /api/auth/anonymLogin`

Parameters:
- `application_key=CHFNKILGDIHBABABA`
- `session_data={"version":3,"client_version":1,"auth_token":"<OneMe token>","device_id":"<deviceId>"}`

Response:
```json
{
  "uid": "<participant_uid>",
  "session_key": "<session_key>",
  "session_secret_key": "<secret>",
  "api_server": "https://calls.okcdn.ru/",
  "external_user_id": "<external_user_id>"
}
```

The `uid` becomes the participant id in signaling. `external_user_id` is the OneMe user ID.

### 2. `GET /api/system/getInfo`

Parameters: `application_key`, `session_key`

Response: `{"serverTime": <ms>}`

### 3. `GET /api/vchat/getConversationParams` (calltaker only)

Parameters: `application_key`, `session_key`, `conversationId=<UUID>`

Response:
```json
{
  "token": "<ws_token>",
  "endpoint": "wss://videowebrtc.okcdn.ru/ws2",
  "turn_server": {
    "urls": ["turn:155.212.199.180:19302", "turn:155.212.197.39:19302"],
    "username": "<timestamp>:<uid>",
    "credential": "<base64>"
  },
  "stun_server": {"urls": ["stun:155.212.199.180:19302"]},
  "client_type": "ONE_ME",
  "device_idx": 0,
  "external_user_type": "ONE_ME",
  "server_time": <ms>,
  "isp_as_no": 8402,
  "isp_as_org": "PVimpelCom",
  "loc_cc": "RU",
  "loc_reg": "48"
}
```

The caller gets equivalent data from the `connection` notification's `conversationParams` field.

### 4. `POST /api/log/externalLog` — telemetry

### 5. `POST /api/vchat/clientStats` — call statistics (sent multiple times)

Contains events: `connection_state_changed`, `ice_candidates_changed`, `websocket_connected`, `signaling_connected`, `call_warmup`, `call_start`, `call_init`, `first_media_received`, `first_media_sent`, `api_call`, `call_finish`.

### 6. `GET /api/settings/get?keys=oneme.webrtc.linux` — returns `{}` (no overrides)

---

## WebRTC Signaling WebSocket (`videowebrtc.okcdn.ru`)

### Connection URL

**Calltaker:**
```
GET /ws2?appVersion=1.44.54144&capabilities=3803F&clientType=ONE_ME
  &conversationId=<UUID>&device=Debian+GNU%2FLinux+13+%28trixie%29
  &entityType=USER&locale=en&peerId=<random_u64>&platform=DESKTOP
  &tgt=accept&token=<token>&userId=<uid>&version=5
```

**Caller:**
```
GET /ws2?appVersion=1.44.54144&capabilities=3803F&clientType=ONE_ME
  &conversationId=<UUID>&device=Debian+GNU%2FLinux+13+%28trixie%29
  &entityType=USER&locale=en&peerId=<random_u64>&platform=DESKTOP
  &token=<token>&userId=<uid>&version=5
```

Differences:
- Calltaker adds `tgt=accept`
- Caller omits `tgt`
- `capabilities=3803F` (hex) for both (vs Firefox which uses `2a03f` in `accepted-call`)
- WebSocket extension: `permessage-deflate; server_max_window_bits=15; client_max_window_bits=15`

---

## Caller Signaling Sequence

```
CLIENT → update-media-modifiers (seq=1)  ← NEW: not in Rust rtc-tun
CLIENT → change-media-settings (seq=2)
CLIENT → change-participant-state (seq=2, {state: {}})  ← NEW: not in Rust rtc-tun
SERVER ← connection (notification)  [TURN creds, participants, conversationId]
SERVER ← settings-update (notification)  [camera/screen quality limits]
SERVER ← response for seqs 1,2,3
SERVER ← registered-peer (notification)  [calltaker peer connected]
SERVER ← ping (text)
CLIENT → pong (text)
SERVER ← accepted-call (notification)  [calltaker accepted]
CLIENT → enable-feature-for-roles (seq=4, feature="ADD_PARTICIPANT", roles=[])
CLIENT → transmit-data (seq=5): SDP offer
CLIENT → transmit-data (seq=6..10): ICE candidates (host, srflx, relay)
SERVER ← response for transmit-data
SERVER ← feature-set-changed / features-per-role-changed
SERVER ← transmitted-data: SDP answer from calltaker
SERVER ← transmitted-data: ICE candidates from calltaker (incl. empty "" end marker)
CLIENT ↔ custom-data every 5s: {sdk: {rtt: <float>, loss: <float>}}
CLIENT → hangup (seq=17, reason="HUNGUP")
SERVER ← response for hangup
```

## Calltaker Signaling Sequence

```
CLIENT → update-media-modifiers (seq=1)  ← NEW: not in Rust rtc-tun
CLIENT → accept-call (seq=2, mediaSettings)
CLIENT → change-media-settings (seq=3)
CLIENT → change-participant-state (seq=4, {state: {}})  ← NEW: not in Rust rtc-tun
SERVER ← connection (notification)  [same as caller side]
SERVER ← settings-update (notification)
SERVER ← transmitted-data: SDP offer from caller (Chrome/VK SDK)
SERVER ← transmitted-data: ICE candidates from caller
CLIENT → transmit-data (seq=5): SDP answer
CLIENT → transmit-data (seq=6): ICE candidate (host only, one candidate)
CLIENT ↔ custom-data every 5s
CLIENT → hangup (reason="HUNGUP")
```

---

## `connection` Notification (full shape)

```json
{
  "stamp": <unix_ns>,
  "peerId": {"id": <own_peer_id_u64>},
  "endpoint": "wss://videowebrtc.okcdn.ru/ws2?conversationId=...&peerId=...&token=...&userId=...&entityType=USER&ispAsNo=...&ispAsOrg=...&locCc=...&locReg=...",
  "conversationParams": {
    "turn": {
      "urls": ["turn:...", "turn:..."],
      "username": "<timestamp>:<uid>",
      "credential": "<base64>"
    },
    "stun": {"urls": ["stun:..."]},
    "serverTime": <ms>,
    "activityTimeout": 120000
  },
  "conversation": {
    "id": "<UUID>",
    "state": "ACTIVE",
    "topology": "DIRECT",
    "participants": [
      {
        "externalId": {"type": "ONE_ME", "id": "<oneme_uid>"},
        "state": "CALLED" | "ACCEPTED",
        "roles": ["CREATOR"],
        "mediaSettings": {"isAudioEnabled": true},
        "peerId": {"id": <peer_id_u64>},
        "responders": [<participant_id>],
        "responderTypes": ["USER"],
        "permissions": ["MUTE_PARTICIPANTS", "REMOVE_JOIN_LINK"],
        "id": <participant_id>
      }
    ],
    "participantsLimit": 1500,
    "features": ["RECORD"],
    "featuresPerRole": {},
    "turnServers": ["turn:..."],
    "options": ["FEEDBACK"],
    "clientType": "ONE_ME",
    "handCount": 0
  },
  "isConcurrent": false,
  "mediaModifiers": {"denoise": true, "denoiseAnn": true},
  "notification": "connection",
  "type": "notification"
}
```

---

## Notifications

| Notification | Direction | When |
|---|---|---|
| `connection` | S→C | First server message (always) |
| `settings-update` | S→C | After connection (camera/screen quality) |
| `registered-peer` | S→C | Caller: when calltaker's WS connects |
| `accepted-call` | S→C | Caller: when calltaker explicitly accepts |
| `transmitted-data` | S→C | When remote peer sends SDP/ICE via transmit-data |
| `feature-set-changed` | S→C | When call features change |
| `features-per-role-changed` | S→C | When per-role features change |
| `ping` (plain text) | S→C | Keepalive |

---

## Commands sent

| Command | Fields | Notes |
|---|---|---|
| `update-media-modifiers` | `mediaModifiers: {denoise, denoiseAnn}` | Sent first, before accept-call/change-media-settings |
| `accept-call` | `mediaSettings` | Calltaker only, seq 2 |
| `change-media-settings` | `mediaSettings` | Both sides |
| `change-participant-state` | `participantState: {state: {}}` | Both sides, after change-media-settings |
| `enable-feature-for-roles` | `feature: "ADD_PARTICIPANT", roles: []` | On `accepted-call` notification |
| `transmit-data` | `participantId, participantType, data: {sdp | candidate}` | SDP and ICE |
| `custom-data` | `data: {sdk: {rtt, loss}}` | Every 5s |
| `hangup` | `reason: "HUNGUP"` | On teardown |

Responses: `{type: "response", response: "<command>", sequence: N, stamp: 0}`

Keepalive: server sends plain-text `ping`, client responds `pong`.

---

## `transmit-data` shapes

SDP:
```json
{
  "command": "transmit-data",
  "sequence": N,
  "participantId": <id>,
  "participantType": "USER",
  "data": {
    "sdp": {"type": "offer"|"answer", "sdp": "<SDP string>"},
    "animojiVersion": 1
  }
}
```

ICE candidate:
```json
{
  "command": "transmit-data",
  "sequence": N,
  "participantId": <id>,
  "participantType": "USER",
  "data": {
    "candidate": {
      "candidate": "candidate:...",
      "sdpMLineIndex": 0,
      "sdpMid": "0"
    }
  }
}
```

End-of-candidates: `candidate: ""` (empty string).

---

## SDP Notes

**Caller (VK SDK/Chrome):**
- Audio: PT 111 (Opus), 63 (RED/111), 110 (telephone-event)
- Video: many H264 profiles, VP9, VP8, AV1, RTX, RED, ulpfec
- Application: DTLS/SCTP datachannel on mid:2
- `setup:actpass`
- `a=group:BUNDLE 0 1 2`

**Calltaker (VK SDK/Chrome):**
- Audio: PT 109 (Opus), with `dred=100;maxaveragebitrate=64000;usedtx=1;useinbandfec=1`
- Video: many codecs same as above, `setup:active`

**The Rust rtc-tun SDP should match this format.** Key: audio PT 111 or 109 depending on role.

---

## ICE / Topology

In the test call (LAN), `call_topology: D` (Direct). Candidate types seen:
- host (192.0.2.x)
- srflx (1.2.3.4 = NAT public IP)  
- relay (155.212.x.x = TURN)

Only one ICE candidate is sent from the calltaker (host only). The caller sends host + srflx + relay candidates.
