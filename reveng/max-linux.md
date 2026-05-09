# Max Linux Client Architecture

## Overview

The Max (VK) Linux desktop client is a Qt 6.9.3 application that splits its call handling into a separate child process (`max-service`). The gRPC-based IPC between the two processes uses the VK Calls SDK (`libcall-service.so`).

The goal being investigated: drive the SDK directly from a Rust `tonic` gRPC client, bypassing the Max GUI, to handle calls independently.

---

## File Layout

```
/usr/share/max/
  bin/
    max                          # Qt GUI application (non-PIE, ~66MB, stripped)
    crashpad_handler             # Google Crashpad OOP crash handler
    max-service/
      bin/
        max-service              # Call backend binary (non-PIE, ~66MB, stripped)
        qt.conf                  # Qt path config for max-service
      lib64/
        libcall-service.so       # VK Calls SDK — gRPC server + call logic (~64MB)
        libcalls_types_converter.so
        libdesktop_utils.so
        libnetwork.so
        liblogger.so
        libfile.so
        libEnhancementLibShared.so  # Audio enhancement (denoisers, etc.)
        libtracernative.so          # Crash reporter (→ sdk-api.apptracer.ru)
        libonnxruntime.so.1.23.2    # ML inference for audio denoising
        libavcodec/avformat/avutil… # FFmpeg
        libpipewire-0.3.so          # PipeWire audio
        libpulse.so                 # PulseAudio fallback
        [Qt6, GLib, OpenSSL, …]
      ann_data/
        audio_denoiser/            # .vkmlmodel files for audio denoising
        nisqa/                     # Audio quality estimation model
  qml/                             # QML components for bin/max UI
  resources/                       # WebEngine .pak resources, v8 snapshot
```

---

## Process Relationship

```
bin/max  (Qt GUI)
  │
  ├─ spawns ──► max-service  [child process]
  │                │
  │                └─ gRPC SERVER on localhost:<rpc-port>
  │                   service: com.vk.calls.sdk.v1.CallAgent
  │
  └─ gRPC CLIENT connects to max-service port
```

- `bin/max` is the gRPC **client** (uses `calls::sdk::Client`)
- `max-service` is the gRPC **server** (uses `calls::sdk::Server`)
- Communication is plaintext gRPC on localhost (no TLS when `--rpc-plaintext` is passed)

### File Descriptors at Startup

Observed fd table when max-service is launched by bin/max:

| fd | target | notes |
|----|--------|-------|
| 0  | `pipe:[N]` (read end) | pipe from bin/max; max-service never reads it |
| 5  | `socket:[N]` | shared with parent (same inode in both processes) |
| 6  | `/proc/<crashpad-pid>/fd` | Crashpad handler's fd dir |
| 7  | `pipe:[N]` (write end) | write end of fd 0 pipe |

`bin/max` additionally holds fd 43 → `anon_inode:[pidfd]` pointing at max-service, used to watch for max-service exit.

---

## max-service CLI Arguments

Observed invocation:
```
max-service --rpc-port 61415 \
            --settings /home/…/.local/share/ONEME/calls/calls-settings \
            --device-id 8811483219817711563 \
            --rpc-plaintext
```

Known arguments (from string analysis of binary):
| Argument | Notes |
|----------|-------|
| `--rpc-port <PORT>` | Port for the gRPC server |
| `--rpc-plaintext` | Use insecure/plaintext gRPC (no TLS) |
| `--settings <PATH>` | Path to calls settings directory |
| `--device-id <ID>` | Device identifier |
| `--owner-pid <PID>` | PID of the owning bin/max process (optional; if absent, likely uses getppid()) |

Settings files live under `<settings>/oneme/settings/app_settings`. The settings contain flags including `rpcSecurityMode`, `rpcEnabled`, `rpcAllowShowForAnyCallId`, `transportPreference`.

---

## SDK Architecture (libcall-service.so)

### Public API split

**`calls::sdk::Server`** — instantiated by max-service; hosts the gRPC server:
```cpp
Server(Config const&)
start()        // starts gRPC server, spawns GrpcListenerImpl thread
stop()
setHealthy(bool)
setUserLoggedIn(optional<string> session_id, string token)
setUserLoggedOut(optional<string> session_id, string token)
setCallStarted(optional<string>, string, CallSessionState)
setCallEnded(optional<string>, string, CallTerminationSide)
setCallFailed(optional<string>, long error_code)
setCallSessionState(optional<string>, string, CallSessionState)
setCallMediaState(optional<string>, string, MediaDeviceType, bool)
setCallEndedWithPostmortem(...)
setCallFailedWithPostmortem(...)
setCallPostmortemCompleted(CallKey, optional<PostCallAction>)
setCallAccepted(string)
setCallJoinLink(string, string)
setNeedCallToken(string, string)
setNeedContactList(string, string, string, ulong)
setNeedContactListPage(string, string, ulong, Direction)
setNeedPartiesInfo(string, vector<string>)
setNeedAddPartiesToCall(string, string, vector<string>, string)
setNeedCreateCallLink(string, string)
setNetworkQualityInfo(string, string, NetworkQualityReport)
setMediaDevices(MediaDeviceType, vector<string>)
setMediaDeviceAdded(MediaDeviceType, string)
setMediaDeviceRemoved(MediaDeviceType, string)
setConfRoomCurrentCalls(string, vector<string>)
setInternalParams(string)
signalOpenChat(string)
signalOpenProfile(string, string)
signalOpenChatOutOfCall(string, string)
// subscribe to incoming requests from the gRPC client (bin/max):
subscribe(function<void(LoginRequestEvent const&)>)
subscribe(function<void(LogoutRequestEvent const&)>)
subscribe(function<void(NewCallRequestEvent const&)>)
subscribe(function<void(EndCallRequestEvent const&)>)
subscribe(function<void(ChangeMyMediaRequestEvent const&)>)
subscribe(function<void(ChangeCallWindowRequestEvent const&)>)
subscribe(function<void(ProcessIncomingCallRequestEvent const&)>)
subscribe(function<void(ConfigEvent const&)>)
subscribe(function<void(CallMetadataEvent const&)>)
subscribe(function<void(CallTokenDataEvent const&)>)
subscribe(function<void(UsersInfoDataEvent const&)>)
subscribe(function<void(ContactListPageDataEvent const&)>)
subscribe(function<void(ContactListPageErrorDataEvent const&)>)
subscribe(function<void(ContactListClosedDataEvent const&)>)
```

**`calls::sdk::Client`** — instantiated by bin/max; connects to the gRPC server:
```cpp
Client(Config const&)
start() / stop() / runTest()
login(LoginArgs, callback)
logout(LogoutArgs, callback)
startCallToUser(StartCallToPeerArgs, callback)
startCallByLink(StartCallByLinkArgs, callback)
startCallWithFastFailure(StartCallWithFastFailureArgs, callback)
startEmptyCall(StartEmptyCallArgs, callback)
processIncomingCall(ProcessIncomingCallArgs, callback)
endCall(EndCallArgs, callback)
changeMyMedia(ChangeMyMediaArgs, callback)
changeCallWindow(ChangeCallWindowArgs, callback)
getStatus(GetStatusArgs, callback)
subscribeToCallStream(CallStreamArgs, callback)          // server-streaming
subscribeToStatusStream(StatusStreamArgs, callback)     // server-streaming
setupDataChannel(callback)
pushConfig(PushConfigArgs, callback)
pushCallMetadata(PushCallMetadataArgs, callback)
cancelOperation(ulong)
provide(DataCallToken, callback)
provide(DataUsersInfo, callback)
provide(DataContactListPage, callback)
provide(DataContactListClosed, callback)
provide(DataContactListPageError, callback)
```

### gRPC Service

Fully qualified service name: **`com.vk.calls.sdk.v1.CallAgent`**

Methods visible in the async method mixin chain (all under the `CallAgent` service):
- `Login` / `Logout`
- `NewCall` / `EndCall`
- `ChangeMyMedia` / `ChangeCallWindow`
- `ProcessIncomingCall`
- `GetCurrentCalls`
- `SetupDataChannel`
- `PushConfig` / `PushCallMetadata`
- `SubscribeToCurrentCallsNotifications` (server-streaming)
- `SubscribeToIncomingCallNotifications` (server-streaming)
- `SubscribeToCallNotifications` (server-streaming)
- `GetStatus` / `SubscribeToStatusStream` (server-streaming)
- `SubscribeToCallStream` (server-streaming)

### Internal components

- **`GrpcListenerImpl`** — spawns a dedicated `std::thread` when `start()` is called; runs the gRPC completion queue loop
- **`ServerImpl`** — concrete `grpc::Service` implementation for `CallAgent`
- **`EventProcessorImpl<GrpcEvent, RequestLoginEvent, …>`** — dispatches all event types; the full event type list is embedded in its template instantiation
- **`calls::tasks::TaskQueue`** — Boost.ASIO-backed task queue; `setStopWorkingSync()` drains and stops it (shutdown path)
- **`IpcServerAdapter`** — mentioned in strings; wraps `ServerImpl` for IPC

### Service lifecycle log strings

```
CallAgent service is created at <address>
CallAgent service is starting...
CallAgent service is UP and RUNNING
CallAgent service is stopping...
CallAgent service is stopped
```

Health events are tracked: `"health event received, is_healthy = "`, `"getStatusRequestCallback: healthy = "`.

---

## AppController (max-service)

Source path (from binary strings): `calls/apps/max-service/AppController.cpp`

Key methods observed:
- `AppController::checkOwnerProcess()` — polls `/proc/<owner-pid>` to verify bin/max is alive; logs `"PID X is not found; it seems like owner process is dead, exiting..."` and exits if the owner is gone
- `AppController::onConnectionReady()` — called when a call connection is established
- `AppController::onCallEnded()` / `onCallFailed()` / `onCallAcceptedByPeer()`
- `AppController::onActiveAccountChanged()`
- `AppController::onMicrophoneStateChanged()` / `onCameraStreamChanged()`
- `AppController::onRoomParticipantsCountUpdated()`
- `AppController::onJoinLinkChanged()`
- `AppController::setPreferredMedia()`
- `AppController::provideCallStartTsIfNeeded()`
- `AppController::onCallPostmortemCompleted()`
- `AppController::onConnectionError()`
- `AppController::onCallFeatureSetChanged()`

---

## Runtime Environment Requirements

### Starting bin/max headlessly (full GUI)

```bash
# D-Bus session (required for gnome-keyring and Qt)
eval $(dbus-launch --sh-syntax)

# Gnome keyring (required — bin/max crashes with "libsecret unavailable" without it)
echo "" | gnome-keyring-daemon --unlock --components=secrets --daemonize

# Virtual framebuffer (required for Qt GUI initialization)
Xvfb :99 -screen 0 1280x1024x24 &
export DISPLAY=:99

# Software rendering (required over SSH, no GPU)
export LIBGL_ALWAYS_SOFTWARE=1

# Mesa spawns ~16 llvmpipe worker threads during first GL context creation
```

### Starting max-service headlessly (gRPC only, CONFIRMED WORKING)

max-service **must run as the display user** (not root) — PipeWire is required for audio
initialization and its socket (`/run/user/<uid>/pipewire-0`) is user-owned.

```bash
# Required environment (Debian VM with existing X session on :10)
export DISPLAY=:10
export XAUTHORITY=/home/user/.Xauthority
export XDG_RUNTIME_DIR=/run/user/1001
export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1001/bus
export HOME=/home/user
export LIBGL_ALWAYS_SOFTWARE=1
export LD_LIBRARY_PATH=/usr/share/max/bin/max-service/lib64

# Launch (as user, not root)
su - user -s /bin/bash -c '
  /usr/share/max/bin/max-service/bin/max-service \
    --rpc-port 62000 \
    --settings /home/user/.local/share/ONEME/calls/calls-settings \
    --device-id 17814744742924580319 \
    --rpc-plaintext
'
```

Key values:
- **device-id**: `12397856234987562345` (from `calls-settings/tracer/deviceid`)
- **settings dir**: `/home/user/.local/share/ONEME/calls/calls-settings`

On startup, max-service reads `MAX.auth.json` and automatically marks userId `000000000`
as logged-in. `GetStatus` returns `healthy=true` within ~5 seconds.

---

## gRPC Client Protocol (confirmed from traffic capture + live testing)

The gRPC sequence to make an outgoing call via max-service:

### 1. Initialization sequence

```
GetStatus                      → StatusReport{healthy=true, userIds=[...]}
SubscribeToStatusNotifications → server-stream of StatusReport
SetupDataChannel               → OPEN FIRST; server immediately sends NeedCallToken
Login(userId)                  → GenericResponse{accepted=true}  (no token needed)
PushConfig(configJSON)         → GenericResponse{accepted=true}
```

### 2. SetupDataChannel bidirectional stream

Server sends `DataChannelRequest`, client sends `DataChannelEvent`.

**NeedCallToken** (sent by server immediately on stream open):
```
server → NeedCallToken{userId.id="000000000"}
client → CallToken{userId, tokenHost="calls.okcdn.ru", tokenValue=<session_key>}
```
Where `session_key` = result of `GET /api/auth/anonymLogin` (obtained by the host app).

**NeedUsersInfo** (sent after NewCall):
```
server → NeedUsersInfo{userIds=[callerUid, calleeUid]}
client → UsersInfo{data=[UserInfo{userId, firstNames, lastNames, callCapability=true}]}
```

### 3. Making an outgoing call

```
NewCall(fastCallSetupInfo)          → GenericResponse{accepted=true}
SubscribeToCallNotifications(callId) → server-stream of CallEvent
```

`NewCallRequest.fastCallSetupInfo.internalCallerParams` is a JSON string obtained from
`GET /api/vchat/start_conversation` (same data as our Rust `start_conversation()` call):

```json
{
  "id": {"internal": <calls_uid_int>, "external": "<oneme_uid_str>"},
  "isConcurrent": false,
  "endpoint": "wss://videowebrtc.okcdn.ru/ws2?userId=...&conversationId=<uuid>&token=...",
  "wtEndpoint": "https://videowebrtc.okcdn.ru:23432/wt?...",
  "clientType": "ONE_ME",
  "turn": {"urls": [...], "username": "<ts>:<uid>", "credential": "<base64>"},
  "stun": {"urls": ["stun:..."]},
  "deviceIdx": 0
}
```

`fastCallSetupInfo.conversationId` = the UUID from the endpoint URL `conversationId=` parameter.

### 4. PushConfig JSON (observed from real bin/max traffic)

```json
{"gcce":true,"gcwre":true,"gc-from-p2p":true,"add-participants-to-gc":true,
 "callEnableIceRenomination":false,"callDontUseVpnForRtp":false,
 "callAllowP2PRelay":true,...}
```

### 5. Token flow summary

| Token | Source | Used for |
|-------|--------|----------|
| `MAX.auth.json` `accessToken` | Settings file | SDK's internal `anonymLogin` call |
| `CallToken.tokenValue` via data channel | Caller provides via `NeedCallToken` response | SDK's `getConversationParams` (calltaker flow) |
| `internalCallerParams.endpoint` token | In-URL, from `start_conversation` | Signaling WebSocket authentication |

The `fake_token` in MAX.auth.json causes SDK's internal `anonymLogin` to fail silently.
For the caller flow this is fine since `internalCallerParams` has all needed credentials.
For the calltaker flow, a real `session_key` must be provided via `NeedCallToken`.

### 6. Python test script

`reveng/grpc_test.py` — demonstrates the full sequence. Run on the VM:
```bash
python3 /tmp/grpc_test.py --session-key <session_key> \
  --caller-uid 000000000 --callee-uid 111111111 \
  --internal-caller-params '<JSON from start_conversation>'
```

---

## TLS Traffic Capture

Max uses OpenSSL for all TLS connections. The Qt GUI loads it via `dlsym()` through its TLS plugin (`libqopensslbackend.so`), while `libcall-service.so` in max-service uses standard PLT calls. A hook shared library handles both cases.

### 1. Build the hook

```bash
gcc -shared -fPIC -o /home/user/hook.so /w/rtc-tun/reveng/hook.c -ldl
```

The hook (`reveng/hook.c`) intercepts `SSL_CTX_new` at two levels:
- **PLT level** — by exporting `SSL_CTX_new` as a real symbol (catches `libcall-service.so` and other standard-linked consumers)
- **`dlsym` level** — by interposing `dlsym` and returning our wrapper when the name `"SSL_CTX_new"` is requested (catches Qt's TLS plugin)

Both paths install `SSL_CTX_set_keylog_callback` on every new TLS context so the session keys are written to `$SSLKEYLOGFILE`.

### 2. Start a packet capture

```bash
sudo tcpdump -i any -w /tmp/capture.pcap &
```

Or target specific hosts to reduce pcap size:

```bash
sudo tcpdump -i any -w /tmp/capture.pcap \
  'host api.oneme.ru or host videowebrtc.okcdn.ru or host calls.okcdn.ru'
```

### 3. Launch Max with the hook

```bash
SSLKEYLOGFILE=/tmp/max.keys LD_PRELOAD=/home/user/hook.so /usr/share/max/bin/max
```

Verify the hook is active — `/tmp/hook_debug.txt` should appear within seconds of the app starting, containing lines like:

```
dlsym: SSL_CTX_new
SSL_CTX_new init: real_new=0x7f... real_keylog=0x7f...
SSL_CTX_new called
```

### 4. Reproduce the call

Make or receive a call. After the call ends, stop the capture:

```bash
sudo kill %1   # or pkill tcpdump
```

### 5. Identify TLS streams

Find the stream number for the signaling WebSocket (`videowebrtc.okcdn.ru`, port 443):

```bash
tshark -r capture.pcap -o "tls.keylog_file:/tmp/max.keys" \
  -Y 'tcp.port == 443' -T fields -e tcp.stream | sort -nu
```

Or list all TLS streams with hosts:

```bash
tshark -r capture.pcap -o "tls.keylog_file:/tmp/max.keys" \
  -Y tls -T fields -e tcp.stream -e ip.dst -e tls.handshake.extensions_server_name \
  | sort -u
```

### 6. Decode the signaling WebSocket

`reveng/decode_ws.py` decodes a single TLS stream (set `pcap`, `keys`, and `stream` at the top):

```python
pcap   = "/tmp/capture.pcap"
keys   = "/tmp/max.keys"
stream = 3   # TCP stream number from step 5
```

Run:

```bash
python3 /w/rtc-tun/reveng/decode_ws.py 2>/dev/null
```

The script:
1. Calls `tshark -z follow,tls,hex,<stream>` to get the raw hex dump
2. Concatenates all bytes per direction (client / server)
3. Skips the HTTP upgrade handshake (`\r\n\r\n` boundary)
4. Parses WebSocket frames, decompressing `permessage-deflate` (RSV1=1) frames with a shared `zlib.decompressobj(wbits=-15)` per direction and a `\x00\x00\xff\xff` trailer per frame
5. Pretty-prints each JSON message

**Note:** The signaling WebSocket always uses `permessage-deflate` compression. Plain text `ping` / `pong` frames are reported but not JSON-parsed.

### 7. Inspect REST calls

For HTTP/1.1 REST calls to `calls.okcdn.ru` and `api.oneme.ru`, use standard tshark HTTP export:

```bash
tshark -r capture.pcap -o "tls.keylog_file:/tmp/max.keys" \
  -Y http -T fields \
  -e http.request.method -e http.request.uri \
  -e http.response.code -e http.file_data
```

---

## Proto Extraction

To get the `.proto` definitions for `com.vk.calls.sdk.v1.CallAgent`:

```bash
# grpc_cli can list services if the server is running with reflection:
grpc_cli ls localhost:61415

# Alternatively, extract from binary with protoc --decode_raw or
# use the grpc_reflection service if enabled
```

No `.proto` files ship in the package — they are compiled into `libcall-service.so`. The service methods listed above were recovered by demangling symbol names from the async method mixin chain in the binary.
