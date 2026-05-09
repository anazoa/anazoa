# Max Android Tracking Inventory

## Scope

This document covers telemetry, analytics, crash/perf reporting, and diagnostic uploads found in `max-26.13.0.apk` and related captures.

It does **not** treat ordinary product traffic such as chat sync to `api.oneme.ru` or call bootstrap/auth requests to `calls.okcdn.ru` as "tracking" unless the payload is explicitly analytical or diagnostic.

Main evidence sources:

- APK static analysis with JADX on `reveng/max-26.13.0.apk`
- Capture `reveng/android/max.pcap` with `reveng/android/keys.log`
- Capture `reveng/android/max-4.pcap` with `reveng/android/keys-4.log`

---

## Reporting Backends

### 1. `sdk-api.apptracer.ru`

Purpose:

- crash/session tracking
- performance metrics
- sample uploads

Observed endpoints:

- `POST /api/crash/trackSession`
- `POST /api/perf/upload`
- `POST /api/sample/initUpload`
- `POST /api/sample/upload`

Primary code paths:

- `defpackage.hg9.a0(q4i)` serializes the shared `SystemState` JSON
- `defpackage.smg` sends session/crash tracking
- `defpackage.lbi` sends performance samples
- `ru.ok.tracer.upload.SampleUploadWorker` sends sample metadata and blobs

### 2. `tracker-api.vk-analytics.ru`

Purpose:

- broad analytics / attribution / device and user telemetry

Observed traffic:

- `POST /v3/` in `max-4.pcap`

Primary code paths:

- `com.my.tracker.core.b`
- `com.my.tracker.core.proto.a`
- `com.my.tracker.core.proto.b`
- `com.my.tracker.core.o.*` data providers

### 3. Call analytics and external logs

Purpose:

- call quality, signaling, negotiation, transport, ML, and SDK diagnostics
- generic external log shipping

Observed traffic:

- `POST /api/vchat/clientStats` to `calls.okcdn.ru`
- `POST /api/v1/report?ver=3` to `trace-flow.ru`

Primary code paths:

- `ru.ok.android.externcalls.analytics.internal.api.CallAnalyticsApiRequest`
- `ru.ok.android.externcalls.analytics.events.*`
- `ru.ok.android.onelog.OneLogApiRequest`

---

## AppTracer Payloads

The app reuses a single `SystemState` object for multiple uploads.

Serializer:

- `defpackage.hg9.a0(q4i)`

Model:

- `defpackage.q4i`

Builder:

- `defpackage.ahb.j(Context)` in fallback decompile

### Shared `SystemState` fields

Top-level fields:

- `versionName`
- `versionCode`
- `packageName`
- `environment`
- `buildUuid`
- `sessionUuid`
- `device`
- `deviceId`
- `vendor`
- `osVersion`
- `inBackground`
- `isRooted`
- `properties`
- `hostedLibrariesInfo`

`properties` map fields confirmed in code:

- `board`
- `brand`
- `cpuABI`
- `device`
- `manufacturer`
- `model`
- `cpuCount`
- `osVersionSdkInt`
- `osVersionRelease`
- optional `processName`
- optional `operatorName`
- optional `installer`
- `date` is added for some upload paths

`hostedLibrariesInfo[]` item fields:

- `packageName`
- `versionName`
- `buildUuid`
- `environment`

### Root reporting in AppTracer

`isRooted` is explicitly serialized into the JSON.

AppTracer-side root check is relatively simple:

- `Build.TAGS` contains `test-keys`
- `/system/app/Superuser.apk` exists
- `/system/xbin/su` exists

There is also emulator-like suppression in the builder path, so some checks are skipped on obvious emulator builds.

### Session / crash tracking payload

Sender:

- `defpackage.smg`

Endpoint:

- `POST https://sdk-api.apptracer.ru/api/crash/trackSession?crashToken=...`

Fields on top of shared `SystemState`:

- `buildUuid`
- `deviceId`
- `sessions`
- optional `drops`

Capture evidence from `max.pcap`:

- decrypted `trackSession` request contained app version, build UUID, device UUID, and session state records

### Performance upload payload

Sender:

- `defpackage.lbi`

Endpoint:

- `POST https://sdk-api.apptracer.ru/api/perf/upload?crashToken=...`

Fields on top of shared `SystemState`:

- `clientTimeUnixNano`
- `samples[]`

Each sample can contain:

- `timeUnixNano`
- `name`
- `value`
- `unit`
- optional `attributes`

Attribute values are serialized as typed JSON values:

- string
- boolean
- long / integer / short / byte
- float / double

### Sample upload payload

Sender:

- `ru.ok.tracer.upload.SampleUploadWorker`

Endpoints:

- `POST /api/sample/initUpload`
- `POST /api/sample/upload`

Metadata fields on init:

- shared `SystemState`
- `feature`
- `sampleSize`
- `sampleFileName`
- optional `attr1`
- optional `attr2`
- optional `tag`

Custom tracer properties can also be merged into the `properties` map before upload.

---

## MyTracker Payloads

This SDK reports substantially more than root state alone.

Packet builder:

- `com.my.tracker.core.proto.a`

Important nested blocks:

- app set ID block
- user info block
- device params block
- ad IDs block
- installed apps block
- remote config block
- custom params map
- stored events and session periods

### Device params block

Collector:

- `com.my.tracker.core.o.m`

Serialized by:

- `com.my.tracker.core.proto.b.a(..., r rVar, ...)`

Confirmed fields:

- package name
- app version code
- app version name
- device identifier from `u0.a(...)`
- device model
- manufacturer
- Android release
- app language
- system language
- timezone display name + timezone ID
- screen width
- screen height
- density DPI
- density
- `xdpi`
- `ydpi`
- `isRooted`
- battery status
- battery percentage
- total app storage
- free app storage
- touchscreen present
- UI mode type

### Root reporting in MyTracker

MyTracker root detection is broader than AppTracer.

Indicators visible in code:

- `test-keys`
- `su` in common locations:
  - `/system/bin/su`
  - `/system/xbin/su`
  - `/data/local/xbin/su`
  - `/data/local/bin/su`
  - `/system/bin/failsafe/su`
  - `/data/local/su`
  - `/su/bin/su`
- `which su`
- Magisk-related mount/artifact markers:
  - `/sbin/.magisk/`
  - `/sbin/.core/mirror`
  - `/sbin/.core/img`
  - `/sbin/.core/db-0/magisk.db`

Encoding:

- `1` rooted
- `0` not rooted
- `-1` collection failed / unavailable

### User info block

Source:

- `com.my.tracker.core.UserInfoState`
- `com.my.tracker.MyTrackerParams`

If the app sets these values, the SDK can report:

- `gender`
- `age`
- `okIds`
- `vkIds`
- `emails`
- `icqIds`
- `customUserIds`
- `phones`
- `vkConnectIds`

### Custom params and identifiers

The packet header may include:

- custom params map
- `android_id`
- `mac`

Note:

- `android_id` and `mac` are taken from the custom params map in the packet builder
- they are omitted in kid mode

### Ad and platform identifiers

Additional providers wired into the packet:

- Google Advertising ID + tracking-enabled flag
- Huawei OAID + tracking-enabled flag
- App Set ID + scope
- Firebase app instance ID

### Installed packages

If `InstalledPackagesProvider` is enabled, MyTracker can send a list of installed **non-system** packages.

Per item:

- package name
- first install time converted to seconds

The SDK hashes the package list and only resends when the set changes.

### Remote config

The MyTracker packet builder also includes:

- the current remote config string

### Stored events and session periods

The SDK does not only send a device header. It also sends:

- stored analytic events
- stored session periods

This is why the `tracker-api.vk-analytics.ru` traffic should be treated as a general analytics sink, not only a device-profile upload.

---

## Call Analytics Payloads

These are separate from MyTracker and AppTracer.

Base request wrapper:

- `ru.ok.android.externcalls.analytics.internal.api.CallAnalyticsApiRequest`

Traits:

- POST
- gzip enabled
- URI is derived from API method name

Observed call analytics endpoint:

- `vchat.clientStats`

Payload envelope:

- method-specific API path
- `items` map

The event classes use a generic schema:

- event name
- numeric or string value
- extra typed item map

### Confirmed event names

From `ru.ok.android.externcalls.sdk.stat.*`, the APK reports events including:

- `call_init`
- `call_start`
- `call_finish`
- `call_warmup`
- `call_accepted_incoming`
- `call_accepted_outgoing`
- `connection_state_changed`
- `audio_error`
- `ice_restart`
- `ice_candidate_add_failed`
- `ice_candidate_gathering_failed`
- `client_requested_server_topology`
- `client_requested_p2p_relay`
- `ml_error`
- `ml_ready_to_use`
- signaling transport events:
  - restart
  - connected
  - reconnected
  - failed by pings
  - failed by exception
  - timeout
- signaling summaries:
  - `signaling_command_summary`
  - `signaling_ping_summary`
- SDP / negotiation error events:
  - `sdp_create_offer`
  - `sdp_create_answer`
  - `sdp_set_local_offer`
  - `sdp_set_remote_offer`
  - `sdp_set_local_answer`
  - `sdp_set_remote_answer`
  - `sdp_set_local_pranswer`
  - `sdp_set_remote_pranswer`
  - `sdp_set_local_rollback`
  - `sdp_set_remote_rollback`

### Confirmed event attributes

Examples visible in code:

- `call_init`:
  - `source`

- `call_start`:
  - JSON `labels` string, containing call-type / warmup labels

- `call_finish`:
  - `reason`
  - `rate_reasons`
  - optional error text as value

- `connection_state_changed`:
  - `connection_state`

- `audio_error`:
  - string value of audio error tuple

- `ice_candidate_add_failed`:
  - `remote_url`

- `ice_candidate_gathering_failed`:
  - `local_address`
  - `remote_url`
  - `transport`

- `client_requested_server_topology`:
  - string value from topology source/type
  - duration as metric value

- `client_requested_p2p_relay`:
  - reason string encoding trigger, threshold, and violation count

- signaling summaries:
  - `api_method`
  - `min_value`
  - `max_value`
  - `avg_value`
  - `median_value`
  - `p95_value`
  - `values_count`

- negotiation errors:
  - JSON value with `error`
  - optional `local` SDP
  - optional `remote` SDP

Capture evidence from `max-4.pcap`:

- decrypted `POST /api/vchat/clientStats` on `calls.okcdn.ru`

---

## External Log Payloads

Generic external log request:

- `ru.ok.android.onelog.OneLogApiRequest`

Observed API method:

- `log.externalLog`

Envelope fields:

- `collector`
- `data.application`
- `data.platform`
- `data.items`

Capture evidence:

- `max-4.pcap` contains `POST /api/v1/report?ver=3` to `trace-flow.ru`
- the exact mapping from `log.externalLog` to `trace-flow.ru` is strongly suggested by traffic and naming, but the final transport routing was not fully reconstructed from code

---

## What Is Explicitly Reported About Root

Two independent reporting paths send rooted/not-rooted state:

### AppTracer path

- field name: `isRooted`
- type: boolean
- included in `SystemState`

### MyTracker path

- field number: device params field `9`
- type: integer enum-like value
- values:
  - `1` rooted
  - `0` not rooted
  - `-1` unknown/error

Current conclusion:

- root status is definitely reported
- I did **not** find code that uses root detection to block login, disable calls, or refuse app startup
- in the APK paths inspected so far, root is used for telemetry/profiling, not enforcement

---

## Ordinary Product APIs Excluded From This Document

These were observed, but are not counted here as tracking by default:

- `api.oneme.ru`
  - main product sync / messaging / call bootstrap data
- `calls.okcdn.ru`
  - `settings/get`
  - `auth/anonymLogin`
- `videowebrtc.okcdn.ru`
  - WebTransport / signaling transport
- `checkip.amazonaws.com`
  - public IP lookup

These matter for protocol analysis, but they are not the telemetry sinks described above.

---

## Confidence Summary

High confidence:

- AppTracer field list and upload purposes
- MyTracker device/user/identifier reporting categories
- root status being reported in two separate systems
- call analytics event family and `vchat.clientStats`

Medium confidence:

- `trace-flow.ru` being the concrete transport behind `log.externalLog`
- full runtime coverage of every optional MyTracker module in this specific app build

Unknown from current evidence:

- exact server-side protobuf schema for `tracker-api.vk-analytics.ru`
- whether app-side code populates every optional MyTracker user identifier field in practice
- whether there are backend-side correlations not visible from client code alone
