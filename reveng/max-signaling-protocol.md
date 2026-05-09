# Max / OneMe WebRTC Signaling Protocol

Documented from direct WebSocket captures made by injecting `ws-capture.js` into Firefox's developer console. Two sessions captured: `ws-calltaker.json` (Firefox receiving a call from Android) and `ws-caller.json` (Firefox placing a call to Android).

Where the Rust source code (`src/signaling.rs`) differs from or adds to what was captured, that is called out explicitly.

---

## Transport

- **Protocol:** WSS (WebSocket over TLS 1.3)
- **Host:** `videowebrtc.okcdn.ru`
- **Path:** `/ws2`
- **Text frames only** (observed). Server sends plain-text `ping` strings (not WebSocket-level Ping frames) every ~5 seconds; client replies with `pong`.

### Connection URL (observed in `connection` notification's `endpoint` field)

```
wss://videowebrtc.okcdn.ru/ws2
  ?conversationId=<uuid>
  &peerId=<numeric-peer-id>
  &token=<base64>
  &userId=<numeric-user-id>
  &entityType=USER
  &ispAsNo=<asn>
  &ispAsOrg=<isp-name>
  &locCc=<country>
  &locReg=<region-code>
```

The `isp*` and `loc*` params vary per-session; presumably added by the server-side signaling token. The source code appends `platform=WEB&appVersion=1.1&version=5&device=browser&capabilities=2A03F&clientType=ONE_ME&tgt=start` — these were not visible in the endpoint URL logged in the capture, so they may be added by the Rust client but not by Firefox, or they appear only in the initial HTTP Upgrade request.

---

## Message structure

### Client → Server (commands)

```json
{
  "command": "<name>",
  "sequence": 42,
  ...command-specific fields
}
```

### Server → Client (responses)

```json
{
  "type": "response",
  "response": "<name>",
  "sequence": 42,
  "stamp": 0
}
```

`stamp` is always `0` in responses. Every command gets an individual response keyed by sequence number. `transmit-data` commands each get their own response immediately.

### Server → Client (notifications)

```json
{
  "type": "notification",
  "notification": "<name>",
  "stamp": <unix-ns>,
  ...notification-specific fields
}
```

`stamp` in notifications is a Unix timestamp in nanoseconds.

---

## Session sequence — calltaker (`ws-calltaker.json`)

### 1. Server sends `connection` notification (immediately on connect, before any client message)

```json
{
  "type": "notification",
  "notification": "connection",
  "stamp": 1700000000086000000,
  "peerId": {"id": <peer_id_1>},
  "endpoint": "wss://videowebrtc.okcdn.ru/ws2?conversationId=...&peerId=...&token=...&userId=...&entityType=USER&...",
  "conversationParams": {
    "turn": {
      "urls": ["turn:155.212.197.40:19000", "turn:155.212.193.12:19000"],
      "username": "<turn_username>",
      "credential": "<turn_credential>"
    },
    "stun": {"urls": ["stun:155.212.197.40:19000"]},
    "serverTime": 1700000000097,
    "activityTimeout": 120000
  },
  "conversation": {
    "id": "<uuid>",
    "state": "ACTIVE",
    "topology": "DIRECT",
    "participants": [
      {
        "externalId": {"type": "ONE_ME", "id": "<participant_external_id_1>"},
        "state": "CALLED",
        "mediaSettings": {"isAudioEnabled": true},
        "id": <participant_id_1>
      },
      {
        "externalId": {"type": "ONE_ME", "id": "<participant_external_id_2>"},
        "state": "ACCEPTED",
        "roles": ["CREATOR"],
        "mediaSettings": {"isAudioEnabled": true},
        "permissions": ["REMOVE_JOIN_LINK", "MUTE_PARTICIPANTS"],
        "id": <participant_id_2>
      }
    ],
    "participantsLimit": 1500,
    "features": ["RECORD"],
    "featuresPerRole": {},
    "turnServers": ["turn:155.212.197.40:19000", "turn:155.212.193.12:19000"],
    "options": ["FEEDBACK"],
    "clientType": "ONE_ME",
    "handCount": 0
  },
  "isConcurrent": false,
  "mediaModifiers": {"denoise": true, "denoiseAnn": true}
}
```

**Note:** The Rust implementation parses this as `ConnectionNotification` and validates that `type == "notification"` and `notification == "connection"` before proceeding. The `conversationParams.turn` block contains TURN credentials, but the Rust implementation fetches those from a separate source (vcp blob for calltaker, REST API for caller) and ignores `conversationParams`.

### 2. Server sends `settings-update` notification (immediately after `connection`)

```json
{
  "type": "notification",
  "notification": "settings-update",
  "stamp": 1700000000086000000,
  "camera": {"maxDimension": 1280, "maxBitrateK": 2000, "degradationPreference": "maintain-framerate"},
  "screenSharing": {"maxDimension": 1920, "maxBitrateK": 3000, "maxFramerate": 30, "degradationPreference": "maintain-resolution"},
  "settings": {
    "badNet": {"rtt": 1000, "loss": 7},
    "goodNet": {"rtt": 600, "loss": 0.5}
  }
}
```

Not handled by the Rust implementation.

### 3. Server sends `registered-peer` notification (when remote peer connects)

```json
{
  "type": "notification",
  "notification": "registered-peer",
  "stamp": 1700000000025000000,
  "peerId": {"id": <peer_id_2>},
  "platform": "ANDROID",
  "clientType": "ONE_ME",
  "participantType": "USER",
  "participantId": <participant_id_2>
}
```

### 4. Server relays remote peer's SDP offer via `transmitted-data` notification

```json
{
  "type": "notification",
  "notification": "transmitted-data",
  "stamp": 1700000000038000000,
  "peerId": {"id": <peer_id_2>},
  "participantType": "USER",
  "participantId": <participant_id_2>,
  "data": {
    "sdp": {"type": "offer", "sdp": "..."}
  }
}
```

### 5. Server relays remote ICE candidates (more `transmitted-data` notifications)

```json
{
  "data": {
    "candidate": {
      "candidate": "candidate:... typ relay ...",
      "sdpMid": "0",
      "sdpMLineIndex": 0
    }
  }
}
```

### 6. Client sends `accept-call` (seq=1)

```json
{"command": "accept-call", "sequence": 1, "mediaSettings": {"isAudioEnabled": true, "isVideoEnabled": false, "isScreenSharingEnabled": false, "isFastScreenSharingEnabled": false, "isAudioSharingEnabled": false, "isAnimojiEnabled": false}}
```

Response:
```json
{"type": "response", "response": "accept-call", "sequence": 1, "stamp": 0, "participantIds": [<participant_id_2>], "participantTypes": ["USER"], "participantDeviceIdxs": [0]}
```

### 7. Client sends `get-rooms` (seq=2), then `change-media-settings` (seq=3), without waiting for responses

```json
{"command": "get-rooms", "sequence": 2, "withParticipants": false}
{"command": "change-media-settings", "sequence": 3, "mediaSettings": {"isAudioEnabled": true, ...all false...}}
```

`get-rooms` response is empty:
```json
{"type": "response", "response": "get-rooms", "sequence": 2, "stamp": 0}
```

`change-media-settings` response:
```json
{"type": "response", "response": "change-media-settings", "sequence": 3, "stamp": 0}
```

### 8. Client sends SDP answer and ICE candidates via `transmit-data` (seq=4..17)

```json
{
  "command": "transmit-data",
  "sequence": 4,
  "participantId": <participant_id_2>,
  "participantType": "USER",
  "data": {"sdp": {"type": "answer", "sdp": "..."}, "animojiVersion": 1}
}
```

Candidates follow with `"data": {"candidate": {"candidate": "...", "sdpMLineIndex": 0, "sdpMid": "0", "usernameFragment": "d0000a30"}}`.

End-of-candidates signaled by an empty candidate string:
```json
{"data": {"candidate": {"candidate": "", "sdpMLineIndex": 0, "sdpMid": "0", "usernameFragment": "d0000a30"}}}
```

Each `transmit-data` gets an individual response: `{"type": "response", "response": "transmit-data", "sequence": N, "stamp": 0}`.

### 9. `feature-set-changed` and `features-per-role-changed` notifications arrive

```json
{"type": "notification", "notification": "feature-set-changed", "stamp": ..., "features": ["RECORD", "ADD_PARTICIPANT"]}
{"type": "notification", "notification": "features-per-role-changed", "stamp": ..., "featuresPerRole": {}}
```

These arrive without any client action in the calltaker session. In the caller session they arrive after `enable-feature-for-roles` (see below).

### 10. `custom-data` command sent periodically (~5s interval)

```json
{"command": "custom-data", "sequence": 18, "data": {"sdk": {"type": "bad-net", "rtt": 2, "loss": 0}}, "participantId": null}
```

Response: `{"type": "response", "response": "custom-data", "sequence": 18, "stamp": 0}`.

This sends network quality stats. The Rust implementation sends hardcoded `{type:"bad-net",rtt:2,loss:0}` every 5 s; the response is discarded.

### 11. Call ends: `hungup` then `closed-conversation` notifications

```json
{
  "type": "notification",
  "notification": "hungup",
  "stamp": 1700000000032000000,
  "reason": "HUNGUP",
  "peerId": {"id": <peer_id_2>},
  "participantType": "USER",
  "participantId": <participant_id_2>,
  "markers": {"SIDE": {"rank": 258}, "GRID": {"rank": 2, "ts": 1700000000043}},
  "deviceCount": 0
}
```

```json
{"type": "notification", "notification": "closed-conversation", "stamp": ..., "reason": "HUNGUP"}
```

---

## Session sequence — caller (`ws-caller.json`)

### 1. Server sends `connection` and `settings-update` (same structure as calltaker)

`connection` notification contains both participants. Caller's own participant has `state: "ACCEPTED"` and `roles: ["CREATOR"]`; calltaker has `state: "CALLED"`.

### 2. Client immediately sends SDP offer via `transmit-data` (seq=1) — no preamble commands

The caller sends the SDP offer as the very first command, before `change-media-settings`:

```json
{
  "command": "transmit-data",
  "sequence": 1,
  "participantId": <participant_id_2>,
  "participantType": "USER",
  "data": {"sdp": {"type": "offer", "sdp": "..."}, "animojiVersion": 1}
}
```

**Note:** The Rust caller implementation sends `change-media-settings` first. The observed order is offer first.

### 3. ICE candidates sent immediately (seq=2..17), then more as STUN/TURN resolve (seq=18..46)

Firefox sends candidates for both `sdpMid: "0"` (audio) and `sdpMid: "1"` (video) in parallel. End-of-candidates is signaled per-mid with an empty candidate string.

### 4. `change-media-settings` sent after initial candidate burst (seq=19)

```json
{"command": "change-media-settings", "sequence": 19, "mediaSettings": {"isAudioEnabled": true, ...all false...}}
```

### 5. `accepted-call` notification arrives when calltaker answers

```json
{
  "type": "notification",
  "notification": "accepted-call",
  "stamp": 1700000000014000000,
  "peerId": {"id": <peer_id_3>},
  "mediaSettings": {"isAudioEnabled": true},
  "capabilities": "3857f",
  "participantType": "USER",
  "participantId": <participant_id_2>
}
```

### 6. Client sends `enable-feature-for-roles` immediately after `accepted-call`

```json
{"command": "enable-feature-for-roles", "sequence": 47, "feature": "ADD_PARTICIPANT", "roles": []}
```

Response (confirmed — this command does get a response):
```json
{"type": "response", "response": "enable-feature-for-roles", "sequence": 47, "stamp": 0}
```

`feature-set-changed` and `features-per-role-changed` notifications follow.

### 7. Remote peer's SDP answer and ICE candidates arrive via `transmitted-data` notifications

Android answers with `usedtx=1` in Opus fmtp and maps `sdes:mid` to extmap ID 3 (not 4 as in its offer).

### 8. `custom-data` sent periodically (seq=48)

Same structure as calltaker. `participantId: null`.

### 9. Client sends `hangup` (seq=49), gets response, session ends

```json
{"command": "hangup", "sequence": 49, "reason": "HUNGUP"}
```

Response: `{"type": "response", "response": "hangup", "sequence": 49, "stamp": 0}`.

---

## SDP carried inside `transmitted-data`

### Firefox calltaker answer (observed)

```
m=audio ... UDP/TLS/RTP/SAVPF 111
a=rtpmap:111 opus/48000/2
a=fmtp:111 maxplaybackrate=48000;stereo=1;useinbandfec=1
a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level
a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid
a=setup:active
```

### Firefox caller offer (observed)

```
m=audio ... UDP/TLS/RTP/SAVPF 109 9 0 8 101
a=rtpmap:109 opus/48000/2
a=fmtp:109 maxplaybackrate=48000;stereo=1;useinbandfec=1
a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level
a=extmap:2/recvonly urn:ietf:params:rtp-hdrext:csrc-audio-level
a=extmap:3 urn:ietf:params:rtp-hdrext:sdes:mid
a=setup:actpass
```

Firefox maps `sdes:mid` to ID 3 in its offer, but to ID 4 in its answer (matching the Android offer). Extension ID assignment is per-side, resolved in the answer.

### Android Chrome answer to Firefox caller offer (observed)

```
a=fmtp:109 minptime=10;useinbandfec=1;dred=100;usedtx=1
a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level
a=extmap:3 urn:ietf:params:rtp-hdrext:sdes:mid
a=setup:active
```

`usedtx=1` confirmed in Android answers as well as offers — DTX is always enabled on the Android side.

---

## TURN credential format

From `conversationParams.turn` in the `connection` notification:

```json
{
  "urls": ["turn:155.212.197.40:19000", "turn:155.212.193.12:19000"],
  "username": "<turn_username>",
  "credential": "<turn_credential>"
}
```

`username` is `<expiry-timestamp>:<user-id>`. This is the standard RFC 5389 long-term TURN credential mechanism (HMAC-SHA1 of the username). The expiry is 86400 seconds from `serverTime`.

---

## Gaps / not observed in captures

- The source code uses `platform=WEB&appVersion=1.1&version=5&device=browser&capabilities=2A03F&clientType=ONE_ME&tgt=start` in the WebSocket URL. These params were not visible in the captured `endpoint` URLs logged by Firefox, so their necessity is unconfirmed from captures.
- `get-rooms` response content: observed to be an empty `{stamp:0, sequence:N, response:"get-rooms", type:"response"}`.
- Call teardown from calltaker side (hangup command) was not captured — calltaker session ended via remote `hungup` notification.
- What `capabilities` field value on `accepted-call` notification means (`"3857f"` seen for Android).
- Whether `custom-data` network stats affect server-side behavior or are purely telemetry.

---

## Implementation status (src/signaling.rs)

| Feature | Status | Notes |
|---|---|---|
| `connection` notification parsing | Implemented, validated | Type and notification name are checked; mismatch returns a clear error |
| `settings-update` notification | Ignored | Not relevant to tunnel operation |
| `accept-call` | Implemented | Calltaker only |
| `get-rooms` | Implemented | Response discarded |
| `change-media-settings` | Implemented | Audio-only settings |
| `transmit-data` / `transmitted-data` | Implemented | SDP and ICE candidate exchange |
| `enable-feature-for-roles` | Implemented, fire-and-forget | Response not awaited |
| `custom-data` (network stats) | Implemented | Sent every 5s during active call; hardcoded `{type:"bad-net",rtt:2,loss:0}` |
| `hangup` | Implemented | 2s response timeout |
| ping/pong keepalive | Implemented | Plain-text frames handled in receive loop |
