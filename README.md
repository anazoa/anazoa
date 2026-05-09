# Anazoa

Anazoa tunnels point-to-point IP traffic over a WebRTC video call on the Russia's government-pushed Max platform. Both ends impersonate an Android Max client. IP packets are carried as tunnel frames embedded in the VP9 video bitstream, paced by Google Chrome libwebrtc's internal scheduler.

Inspired by the [Protozoa](https://dl.acm.org/doi/10.1145/3372297.3417874) work.

Expected tunneling bandwidth for a 720p video call is around 2 Mbps.

## Usage

Two peers need to be configured, local and remote.

1. Use `anazoa.toml.sample` as a template for creating two `anazoa.toml` configuration files (it's advised to tweak `fingerprint` section) and for each peer run
```
cp anazoa.toml.sample anazoa.toml
edit anazoa.toml
anazoa-auth -c anazoa.toml login --phone +71234567890
```

Phone numbers must be different for the two peers.

Update both configuration files with:
  - `token`, `signaling-user-id` and `remote-peer-id` parameters you got after successful authentication,
  - `media` parameter pointing to an Opus audio file -- some audiobook will do.

2. Run on both sides
```
anazoa-tun -c anazoa.toml
```
and then configure newly created `tun0` (the name chosen via `tun-name` parameter) network interfaces (see `scripts/setup-tun-test.sh` for a sample).

3. Make a call
```
anazoa-ctl -s /run/anazoa.sock call
```
After a WebRTC connection establishes (the peer on the other side answers automatically), the tunnel is ready to be used.

To hang up the call run on any side
```
anazoa-ctl -s /run/anazoa.sock hangup
```
