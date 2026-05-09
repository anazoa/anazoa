# Max Android OneMe Protocol

## Scope

This note describes the Android app protocol used on the long-lived TLS connection to:

- `api.oneme.ru:443`

It is based on:

- static inspection of `reveng/base.apk`
- decrypted traffic from `reveng/android/max-5.pcap`
- the local parser in `src/bin/android_oneme.rs`

Important distinction:

- this is not the web client websocket protocol documented in `reveng/max-oneme-protocol.md`
- this is not the call signaling protocol on `videowebrtc.okcdn.ru`
- this is the Android app's own binary RPC/session protocol

---

## Short Answer

Android OneMe keeps a long-lived TLS session to `api.oneme.ru` and exchanges framed binary packets.

Each packet contains:

- protocol version
- command type
- sequence number
- opcode
- payload length / compression info
- payload body

The payload body is a MessagePack-like map. Small packets are usually plain MessagePack. Larger packets are often compressed.

For outgoing calls, the app does not need `calls.okcdn.ru vchat.startConversation`. Instead, it sends a `VIDEO_CHAT_START_ACTIVE` request over this `api.oneme.ru` stream and receives `internalCallerParams`, including:

- `endpoint` for websocket signaling
- `wtEndpoint` for QUIC/WebTransport signaling
- TURN/STUN information

That is the bootstrap source seen in `max-5.pcap`.

---

## Transport

Observed in `reveng/android/max-5.pcap`:

- host: `api.oneme.ru`
- transport: TLS over TCP
- stream: long-lived application session
- capture stream of interest: `tcp.stream == 1`

After TLS decryption, the application data is not HTTP and not websocket frames. It is a custom binary framing format.

---

## Packet Format

The packet structure is implemented in `n0d.java` from `base.apk`.

Header layout:

- `1 byte` `ver`
- `1 byte` `cmd`
- `2 bytes` `seq`
- `2 bytes` `opcode`
- `4 bytes` payload-length field

The last 4 bytes are split as:

- high byte: compression / compression-factor field, called `cof` in code
- low 24 bits: payload length on the wire

So the full header size is:

- `10 bytes`

The payload follows immediately after the header.

Observed protocol version in `max-5`:

- `ver = 10`

This differs from the web websocket protocol, where the observed `ver` was `11`.

---

## Payload Encoding

The request builder in `n0d.a(...)` serializes the request body from an `mw` map with `n1h.w0(...)`.

In practice, the payloads recovered from `max-5` decode as MessagePack maps containing:

- strings
- integers
- booleans
- arrays
- nested maps
- binary blobs

Examples from the capture:

- hello payload with `userAgent`, `deviceId`
- sync payload with `token`, `chatsSync`, `contactsSync`
- call-start payload with `conversationId`, `calleeIds`, `internalParams`

Compression behavior:

- small packets are often uncompressed
- larger packets often use the nonzero `cof` path
- the code strongly suggests LZ4 for normal compression and a separate zstd path for a special marker

For reverse-engineering, the practical rule is:

- parse the 10-byte header first
- then decode or decompress the payload body
- then interpret the result as a MessagePack map

---

## Observed `cmd` Semantics

The exact semantics are not fully proven, but the observed behavior in `max-5` and the read path in `xn.java` are consistent with:

- `cmd = 0`: request or server push
- `cmd = 1`: normal response / acknowledgement
- `cmd = 3`: error response

Observed example:

- client sends `cmd=0 seq=25 opcode=78`
- server replies with `cmd=1 seq=25 opcode=78`

So request/response matching uses at least:

- same `seq`
- same `opcode`

---

## Observed Android Opcodes

The following opcodes were decoded directly from `max-5.pcap`.

| Opcode | Name / payload shape | Likely meaning |
|---|---|---|
| `1` | `{"interactive": true/false}` | heartbeat / activity state |
| `5` | `{"events":[...]}` | app event upload |
| `6` | hello with `userAgent`, `deviceId` | session hello |
| `19` | sync payload with token and sync cursors | authenticated app/session sync |
| `22` | push token / options | push registration state |
| `27` | asset sync payloads like sticker/reaction types | asset sync |
| `35` | `{"contactIds":[...]}` | contact / presence fetch |
| `48` | `{"chatIds":[...]}` | chat fetch |
| `50` | read marker payload | read-state update |
| `75` | `{"chatId":...,"subscribe":true/false}` | subscribe / unsubscribe chat updates |
| `78` | `VIDEO_CHAT_START_ACTIVE` | outgoing call bootstrap |
| `128` | message/chat delta | new message or message update |
| `162` | complain sync payload | complaints/moderation sync |
| `180` | `{"messageIds":[...],"chatId":...}` | reactions / message metadata fetch |
| `272` | `{"folderSync":...}` | folder sync |
| `300` | folder-related payload | folder/config operation |

Only `78` is analyzed in depth below.

---

## Session Bootstrap

The beginning of the Android session in `max-5` looks like:

1. Client hello, `opcode 6`
2. Interactive state, `opcode 1`
3. Authenticated sync, `opcode 19`
4. Push token / push options, `opcode 22`
5. Navigation or app events, `opcode 5`

The hello payload contains fields like:

- `deviceType: "ANDROID"`
- `appVersion: "26.13.0"`
- `osVersion: "Android 16"`
- `timezone: "Europe/Moscow"`
- `screen: "420dpi 420dpi 1080x2340"`
- `deviceName: "samsung SM-A405FM"`
- `deviceId`
- `clientSessionId`

The sync payload contains fields like:

- auth `token`
- `bannersSync`
- `draftsSync`
- `callsSync`
- `contactsSync`
- `chatsSync`
- `presenceSync`
- `interactive`
- `lastLogin`

So this protocol is not just call-related. It appears to be the main Android app session protocol.

---

## Outgoing Call Bootstrap

This is the most important finding from `max-5`.

### Request

The app sends:

- `cmd=0`
- `seq=25`
- `opcode=78`

Opcode `78` is `VIDEO_CHAT_START_ACTIVE` in `svc.java`.

Decoded request payload:

```json
{
  "conversationId": "00000000-0000-0000-0000-000000000000",
  "calleeIds": [111111111],
  "internalParams": "{\"platform\":\"ANDROID\",\"sdkVersion\":\"0.1.10.1\",\"clientAppKey\":\"CGPGAGLGDIHBABABA\",\"deviceId\":\"bbaaaaddff0000dd\",\"protocolVersion\":5,\"onlyAdminCanRecord\":false,\"waitForAdmin\":false,\"capabilities\":\"3c57f\"}",
  "isVideo": false
}
```

That matches the static request builder in `wac.java`.

### Response

The server returns:

- `cmd=1`
- `seq=25`
- `opcode=78`

The raw decrypted response in `max-5` clearly contains:

- `conversationId`
- `internalCallerParams`
- `rejectedParticipants`

Inside `internalCallerParams`, the response contains:

- `isConcurrent`
- `endpoint`
- `wtEndpoint`
- `clientType`
- `turn`
- `stun`

Recovered fields include:

- websocket signaling endpoint:
  - `wss://videowebrtc.okcdn.ru/ws2?...`
- WebTransport endpoint:
  - `https://videowebrtc.okcdn.ru:23432/wt?...`
- `clientType: "ONE_ME"`
- TURN/STUN addresses on `155.212.193.x:19302`

This is the server-side source of the signaling bootstrap in `max-5`.

That is why the capture can show:

1. `VIDEO_CHAT_START_ACTIVE` on `api.oneme.ru`
2. then QUIC to `videowebrtc.okcdn.ru:23432`

without any visible `calls.okcdn.ru startConversation` response.

---

## Relationship To Signaling

High-level flow for the observed outgoing Android call:

1. Existing app session on `api.oneme.ru`
2. Send `VIDEO_CHAT_START_ACTIVE` over the binary app protocol
3. Receive `internalCallerParams`
4. Extract:
   - `endpoint`
   - `wtEndpoint`
   - TURN/STUN data
5. Choose signaling transport
6. Connect to `videowebrtc.okcdn.ru`

In `max-5`, the chosen transport was QUIC/WebTransport:

- `videowebrtc.okcdn.ru:23432`

So this protocol is upstream of signaling. It is the control-plane mechanism that gives the app the signaling endpoints and credentials.

---

## Relationship To The Web Protocol

There is a related but different protocol used by the web client:

- documented in `reveng/max-oneme-protocol.md`

Key differences:

- web client uses WSS JSON messages
- Android app uses TLS + custom binary framing
- web captures observed `ver = 11`
- Android captures observed `ver = 10`

The logical operations overlap:

- hello
- interactive/heartbeat
- sync
- incoming/outgoing call notifications

But the wire format is different.

---

## What Is Known vs Unknown

Known:

- Android uses a custom framed binary RPC/session protocol on `api.oneme.ru`
- header format is known
- payloads are MessagePack-like maps
- `VIDEO_CHAT_START_ACTIVE` request and response are present in `max-5`
- the response contains both `endpoint` and `wtEndpoint`
- this is the source of QUIC/WebTransport bootstrap for the observed outgoing call

Not fully resolved:

- the full opcode catalogue
- the complete compression rules for every `cof` value
- exact semantics of every `cmd` value outside the observed request/response cases
- a full robust decoder for all packet types and payload variants

---

## Practical Reverse-Engineering Notes

The current local helper:

- `src/bin/android_oneme.rs`

is enough to:

- parse the packet header
- split packets from the decrypted stream
- decode many payloads into readable maps

The raw `tshark -z follow,tls,hex,...` output is still useful because it exposes the decrypted bytes directly, which helped confirm the `opcode 78` response and the embedded `internalCallerParams` JSON.

---

## Bottom Line

The Android OneMe app protocol is the main binary session protocol to `api.oneme.ru`.

For calls, it acts as the bootstrap/control channel, not the media channel and not the final signaling channel.

In the observed outgoing call:

- `api.oneme.ru` provided the call bootstrap
- that bootstrap included both websocket and WebTransport signaling URLs
- the app then used `wtEndpoint`
- QUIC/WebTransport signaling followed immediately after
