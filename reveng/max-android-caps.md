# Max Android Capabilities

## Scope

This note documents how the OneMe Android calls SDK builds and transmits the `capabilities` value seen in signaling URLs and call bootstrap requests, for example:

- `appVersion=sdk-0.1.10.1`
- `capabilities=3c57f`
- `clientType=ONE_ME`

Main evidence sources:

- JADX output for `reveng/max-26.13.0.apk`
- static inspection of the embedded calls SDK classes

---

## Short Answer

`capabilities` is a hex-encoded bitmask built from `ClientCapabilities`.

Build path:

1. Start from `ClientCapabilities.getDefault()` unless the app injects an override.
2. Apply a few SDK config booleans in `ConversationFactoryParams.getBaseBuilder(...)`.
3. Apply a final per-user runtime adjustment in `ConversationImpl.getCapabilitiesForCurrentUser(...)`.
4. Serialize the result with `ClientCapabilities.getHexValueString()`.
5. Send it both:
   - in the start-call API request
   - in the signaling / WebTransport URL query string

For the observed value `3c57f`, the final mask is:

- default SDK mask
- minus `VMOJI`
- plus `SESSION_STATE_UPDATES`
- plus `WAIT_FOR_ADMIN`

The exact app-side callsite that adds the last two bits was not recovered cleanly from decompiled Java, but the final runtime path is clear.

---

## Where The Mask Is Defined

The capability model is defined in:

- [ClientCapabilities.java](/tmp/jadx-max-full/sources/ru/ok/android/externcalls/sdk/capabilities/ClientCapabilities.java)

Important methods:

- `getValue()`
- `getHexValueString()`
- `getDefault()`
- `from(int value)`
- `set(...)`
- `plus(...)`
- `minus(...)`

`getHexValueString()` is simply:

- `Integer.toHexString(getValue())`

So `capabilities=3c57f` is just the hex representation of the final integer bitmask.

---

## Capability Bits

Defined enum values and bit positions:

- `SCREEN_TRACK_PRODUCER` bit `0`
- `VIDEO_TRACKS` bit `1`
- `WAITING_HALL` bit `2`
- `FILTER_DEFAULTS` bit `3`
- `SCREEN_TRACK_CONSUMER` bit `4`
- `ADMIN_MUTE_NOTIFY` bit `5`
- `WATCH_MOVIE` bit `6`
- `SESSION_ROOMS` bit `8`
- `VMOJI` bit `9`
- `CALL_TO_CONTACTS` bit `10`
- `AUDIENCE_MODE` bit `11`
- `SESSION_STATE_UPDATES` bit `14`
- `ADD_PARTICIPANT` bit `15`
- `USE_P2P_RELAY` bit `16`
- `WAIT_FOR_ADMIN` bit `17`

---

## SDK Default Mask

The SDK default comes from `ClientCapabilities.getDefault()` in [ClientCapabilities.java](/tmp/jadx-max-full/sources/ru/ok/android/externcalls/sdk/capabilities/ClientCapabilities.java).

Default-enabled capabilities:

- `SCREEN_TRACK_PRODUCER`
- `VIDEO_TRACKS`
- `WAITING_HALL`
- `FILTER_DEFAULTS`
- `SCREEN_TRACK_CONSUMER`
- `ADMIN_MUTE_NOTIFY`
- `WATCH_MOVIE`
- `SESSION_ROOMS`
- `VMOJI`
- `CALL_TO_CONTACTS`
- `ADD_PARTICIPANT`
- `USE_P2P_RELAY`

Default hex mask:

- `1877f`

---

## Builder-Time Adjustment

The base builder logic is in:

- [ConversationFactoryParams.java](/tmp/jadx-max-full/sources/ru/ok/android/externcalls/sdk/ConversationFactoryParams.java:73)

If `clientCapabilities == null`, the SDK starts with the default mask and applies these config booleans:

- `WAITING_HALL` from `isWaitingRoomActivated`
- `SESSION_ROOMS` from `isSessionRoomsFeatureEnabled`
- `FILTER_DEFAULTS` from `isSignalingDefaultValuesFilteringEnabled`
- `AUDIENCE_MODE` from `isAudienceModeEnabled`

Relevant defaults in the same class:

- `appVersion = "sdk-0.1.10.1"`
- `isWaitingRoomActivated = true`
- `isSessionRoomsFeatureEnabled = true`
- `isSignalingDefaultValuesFilteringEnabled = true`
- `isAudienceModeEnabled = false`

So with no app override, this stage still does not add:

- `SESSION_STATE_UPDATES`
- `WAIT_FOR_ADMIN`

---

## Runtime Adjustment

The final runtime adjustment is in:

- [ConversationImpl.java](/tmp/jadx-max-full/sources/ru/ok/android/externcalls/sdk/ConversationImpl.java:1714)

Method:

- `getCapabilitiesForCurrentUser(ClientCapabilities clientCapabilities, wu1 wu1Var, boolean z)`

What it does:

- forces `VIDEO_TRACKS` on only if `wu1Var.j > 0`
- disables `VMOJI` unless both:
  - the input mask already has `VMOJI`
  - the method argument `z` is `true`

The actual call path is:

- [ConversationImpl.java](/tmp/jadx-max-full/sources/ru/ok/android/externcalls/sdk/ConversationImpl.java:1446)
- [ConversationImpl.java](/tmp/jadx-max-full/sources/ru/ok/android/externcalls/sdk/ConversationImpl.java:1166)

At the callsite, the method is invoked with `z = false`, so `VMOJI` is stripped from the final signaling mask.

---

## Where The Final Mask Is Stored

`ConversationImpl` stores the finalized signaling capabilities here:

- [ConversationImpl.java](/tmp/jadx-max-full/sources/ru/ok/android/externcalls/sdk/ConversationImpl.java:1166)

```java
this.clientCapabilities = configureSignalingCapabilities(
    conversationParticipant,
    conversationBuilder.clientCapabilities
);
```

That final `this.clientCapabilities` is then reused in two places.

### 1. Start-call API request

- [ConversationImpl.java](/tmp/jadx-max-full/sources/ru/ok/android/externcalls/sdk/ConversationImpl.java:1597)
- [StartConversation.java](/tmp/jadx-max-full/sources/ru/ok/android/externcalls/sdk/api/request/StartConversation.java:44)

The SDK sends:

- `capabilities=<hex mask>`
- `waitForAdmin=<boolean>`

### 2. Signaling / WebTransport URL

- [ConversationImpl.java](/tmp/jadx-max-full/sources/ru/ok/android/externcalls/sdk/ConversationImpl.java:2371)
- [s5h.java](/tmp/jadx-max-full/sources/defpackage/s5h.java:56)

That path appends:

- `version`
- `capabilities`
- `device`
- `platform`
- `clientType`
- `appVersion`
- `osVersion`
- ISP / geolocation fields

This is the source of URLs that contain:

- `appVersion=sdk-0.1.10.1`
- `capabilities=3c57f`
- `clientType=ONE_ME`

---

## `clientType=ONE_ME`

`clientType` is carried separately from `capabilities`.

The enum-like mapping is visible in:

- [h6j.java](/tmp/jadx-max-full/sources/defpackage/h6j.java)

Observed string mapping:

- `1 -> UNSPECIFIED`
- `2 -> ONE_VIDEO`
- `3 -> ONE_ME`

So `clientType=ONE_ME` is not derived from the capability mask.

---

## Decoding `3c57f`

Observed mask:

- `3c57f`

Set bits:

- `0`
- `1`
- `2`
- `3`
- `4`
- `5`
- `6`
- `8`
- `10`
- `14`
- `15`
- `16`
- `17`

Decoded capabilities:

- `SCREEN_TRACK_PRODUCER`
- `VIDEO_TRACKS`
- `WAITING_HALL`
- `FILTER_DEFAULTS`
- `SCREEN_TRACK_CONSUMER`
- `ADMIN_MUTE_NOTIFY`
- `WATCH_MOVIE`
- `SESSION_ROOMS`
- `CALL_TO_CONTACTS`
- `SESSION_STATE_UPDATES`
- `ADD_PARTICIPANT`
- `USE_P2P_RELAY`
- `WAIT_FOR_ADMIN`

Not present in `3c57f`:

- `VMOJI`
- `AUDIENCE_MODE`

---

## Relationship To The SDK Default

SDK default:

- `1877f`

Observed final mask:

- `3c57f`

Difference:

- remove `VMOJI` (`0x200`)
- add `SESSION_STATE_UPDATES` (`0x4000`)
- add `WAIT_FOR_ADMIN` (`0x20000`)

Equivalent arithmetic:

- `0x1877f - 0x200 + 0x4000 + 0x20000 = 0x3c57f`

This matches the code:

- `VMOJI` removal is explicitly explained by `ConversationImpl.getCapabilitiesForCurrentUser(...)`
- the two added bits must already be present before that runtime adjustment

---

## `WAIT_FOR_ADMIN` Bit Versus `waitForAdmin` Boolean

These are related but distinct.

The SDK sends `waitForAdmin` as a dedicated boolean in the start-call request:

- [StartConversation.java](/tmp/jadx-max-full/sources/ru/ok/android/externcalls/sdk/api/request/StartConversation.java:44)

The SDK also exposes a separate capability bit:

- `ClientCapabilities.Capability.WAIT_FOR_ADMIN`

There is also a call-option enum entry:

- [ga1.java](/tmp/jadx-max-full/sources/defpackage/ga1.java)

with:

- `WAIT_FOR_ADMIN`
- `ADMIN_IS_HERE`

So the protocol surface uses more than one signal around this feature:

- a call option / room state concept
- a request boolean
- a capability bit

They should not be treated as the same field.

---

## What Is Still Unresolved

I did not find a clean app-side Java/Kotlin callsite that explicitly does one of these before `ConversationImpl` is created:

- `setClientCapabilities(...)`
- `plus(SESSION_STATE_UPDATES)`
- `plus(WAIT_FOR_ADMIN)`

So the unresolved point is narrow:

- the final runtime value `3c57f` is clear
- the SDK assembly path is clear
- `VMOJI` removal is clear
- but the exact upstream source of the two extra added bits was not recovered cleanly from decompiled app code

The most likely explanation is:

- the app injects a custom `ClientCapabilities` override before `ConversationImpl`

---

## Practical Conclusion

When you see:

- `appVersion=sdk-0.1.10.1`
- `capabilities=3c57f`
- `clientType=ONE_ME`

it means:

- the app is using the embedded calls SDK default app version string
- the client type is OneMe
- the capabilities mask is the SDK default, adjusted for current-user signaling, with:
  - `VMOJI` removed
  - `SESSION_STATE_UPDATES` enabled
  - `WAIT_FOR_ADMIN` enabled

