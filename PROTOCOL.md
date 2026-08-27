# CarPlay wire protocol — reverse-engineered notes

Everything here that the Accessory Interface Specification R29 §30 leaves to Apple's
"Communication Plug-in" was recovered from two Carlinkit artefacts in
`../wireless-carplay-dongle-reverse-engineering/`:

| Source | What it is | Role |
| --- | --- | --- |
| `Firmware/U2AW/_AUTOKIT/2024.01.10.0953/Decrypted/fakeiOSDevice` | Hewei build of Apple's CarPlay **device** sources (ARM32, unpacked) | what carplayd must become |
| `.../Decrypted/AppleCarPlay` | Apple's AirPlay/CarPlay **receiver** (Thumb-2, unpacked) | the accessory side |
| `Extracted/*/var/log/box_last_reboot.log` | full session traces, wired **and** wireless | ground truth |

Most other firmware builds are UPX-packed with an obfuscated magic (`0x22552255`) *and*
AES-encrypted through the i.MX6 CAAM (`/dev/hwaes`); unpacking those needs the physical
dongle. The `Decrypted/` directory above escaped both.

The logs capture a real iPhone 7 (iOS 14.6, `sourceVersion 545.22.21`) and a real
iPhone X (iOS 15.2, `600.8.1`) driving the dongle, plus the dongle driving a VW head
unit over USB/NCM. So device-side plists below are **from real iPhones**, not from
Hewei's reimplementation.

Confidence is marked per item: **[V]** verified in disassembly or a log dump,
**[I]** inferred.


## 1. Timing / NTP  — `AirPlayNTPServer.c`

We are the **server**; the accessory is the client. It is Apple's `RTPTimeSyncPacket`,
a 32-byte RTP-wrapped NTP exchange — *not* a 48-byte RFC 5905 packet. **[V]**

```
                 request (accessory -> device)     response (device -> accessory)
 [0]             0x80                              0x80
 [1]             0xD2  (marker | PT 82)            0xD3  (marker | PT 83)
 [2..4]          seq BE16, ignored                 0x0007, constant
 [4..8]          ignored                           BE32 seconds of transmit time
 [8..16]         ignored                           <- verbatim copy of request[24..32]
 [16..24]        ignored                           receive  time,  NTP64 BE
 [24..32]        client transmit time              transmit time,  NTP64 BE
```

Validation is two bytes only: `packet[0] >> 6 == 2`, `packet[1] == 0xD2`. Everything
else in the request is ignored except `[24..32]`, copied out **unbyteswapped**. Reply
with `sendto` to the datagram's source; length must be 32.

Clock: `clock_gettime(CLOCK_MONOTONIC)` in nanoseconds, plus the literal
`0x1ea7e79c20d10000` = `2_208_988_800 * 10^9` (the NTP 1900→1970 offset in ns), then
converted to NTP64 as `(ns/1e9) << 32 | (ns%1e9) * 2^32 / 1e9`. **[V]**

That means the shipping product presents a wall clock reading *1 Jan 1970 + uptime* and
head units accept it. The absolute epoch is irrelevant; only monotonicity and internal
consistency matter. Use one clock for NTP responses, video PTS and `/feedback`.

The receive timestamp is a **kernel** timestamp: `SocketRecvFrom` uses `recvmsg` with a
256-byte control buffer and walks the cmsgs for `cmsg_level == SOL_SOCKET (1)` /
`cmsg_type == SCM_TIMESTAMP (29)`, then rebases the `timeval` into the monotonic
timebase with a `gettimeofday`/`UpTicks` pair. **[V]** Not optional polish — Apple does it.


## 2. Screen stream — 128-byte header

Confirmed from both directions: `_AirPlayReceiverSessionScreen_ProcessFrame`
(receiver) and `VideoSource.c` (sender). The header is **sent in the clear**. **[V]**

```
 [0..4]    bodySize      LE32  — bytes of body that follow, tag included
 [4]       opcode        u8
 [5..8]    smallParams   u8[3]
 [8..16]   timestamp     LE64, NTP64 format, same clock as the NTP server
 [16..128] params        u8[112], opcode-specific
```

The receiver's struct is `{u32 bodySize; u8 opcode; u8 smallParams[3]; u8 params[120];}`
at `+72/+76/+77/+80` — 4+1+3+120 = 128, mapping 1:1 onto the wire with no padding. **[V]**

Opcodes, from the dispatch: **[V]**

| Opcode | Meaning |
| --- | --- |
| `0` | video frame data |
| `1` | `VideoConfig` — AVCC record + display geometry |
| `2` | accepted, no action |
| `4`, `5` | accepted, no action |
| other | `### Unknown screen opcode: %u` |

### VideoConfig (opcode 1)

`params` carries float32 geometry; observed live for an 800x480 head unit:

```
header[8..16]   timestamp
header[16..20]  f32 width   = 800.0
header[20..24]  f32 height  = 480.0
header[32..40]  f32 pair    (view area)
header[40..48]  f32 pair    = 800.0, 480.0
header[48..56]  f32 pair
header[56..64]  f32 pair    = 800.0, 480.0
```

The sender writes five float pairs at header offsets 16/20, 32/36, 40/44, 48/52,
56/60. **[V]** Which pair is display vs. view area vs. safe area is **[I]** — the
`_processScreenSetup` path also logs `set viewArea:`/`set safeArea:`.

`header[6]` is a flags byte; the receiver does `ubfx r3, r3, #1, #1` on it. Observed
value `0x16`. **[V]** for the bit extraction, **[I]** for what it means.

Body is the `AVCDecoderConfigurationRecord`, `bodySize` 39 in every capture:

```
01 64 00 1f ff e1 00 14  <20-byte SPS>  01 00 04  <4-byte PPS>  02 00 00 00
```

- `64` = High Profile, `1f` = Level 3.1, `ff` → **4-byte NAL length prefixes**
- a real iPhone emits SPS/PPS with NAL headers `0x27`/`0x28` (`nal_ref_idc = 1`);
  x264 in the dongle emits `0x67`/`0x68` (`nal_ref_idc = 3`). Both are accepted. **[V]**
- the trailing `02 00 00 00` is 4 bytes past the end of the standard record. **[I]** as to purpose.

### Video frames (opcode 0)

Raw header prefix off the wire, from a live `Late frame` dump:

```
27 29 00 00   00 00 00 00   d0 ef fb 76 11 09 00 00
^bodySize     ^opcode+small ^timestamp LE64
```

Consecutive frame timestamps differ by exactly `0x04444391`, i.e.
`0x04444391 / 2^32 = 16.64 ms` → 60 fps. This is what proves the timestamp is NTP64
fixed-point and not nanoseconds. **[V]**

### Media encryption

```
chacha20poly1305_init(ctx, key = DataStreamKey, nonce = counter)
chacha20poly1305_add_aad(ctx, header, 128)          <- AAD is the whole 128-byte header
chacha20poly1305_decrypt(ctx, src, bodySize - 16, dst)
chacha20poly1305_verify(ctx, dst, src + bodySize - 16)
counter += 1                                         <- 8-byte LE increment with carry
```

**[V]** for all four. So:

- wire body is exactly `bodySize` bytes = `bodySize - 16` ciphertext + 16-byte tag
- **the 128-byte header is the AAD**, not part of the ciphertext
- the nonce is an **8-byte little-endian counter** starting at 0, bumped once per
  frame for the life of the stream (matches the control channel's own scheme)
- a per-session flag can disable encryption entirely
  (`### screen frame data no crypto!!!!`)

### Key derivation — `streamConnectionID` signedness

```c
asprintf(&salt, "%s%llu", "DataStream-Salt", streamConnectionID);
HKDF-SHA512(secret, salt, strlen(salt), "DataStream-Output-Encryption-Key", 32, out);
HKDF-SHA512(secret, salt, strlen(salt), "DataStream-Input-Encryption-Key",  31, out);
```

**[V]** — `%llu`, **unsigned**, and the salt is used without its NUL.

This matters: real iPhones put a **signed** int64 in the plist and it is often negative
(iOS 14.6 sent `-8950532481248312265`). The salt formats those bits as unsigned. So
decode the plist value as `i64` and cast bit-preserving to `u64` before formatting.
`SessionKeys::data_stream(&secret, id: u64)` in `src/pairing/keys.rs` is already right;
the hazard is entirely in the plist decode path. Generating positive IDs ourselves
sidesteps it, but we still have to parse whatever the accessory echoes.


## 3. Control channel

`/pair-setup` (x3, HomeKit type 0) → `/pair-verify` (x2) → encrypted. **[V]**, from
`[AirPlayPair] Control pair-setup HK, type 0` three times, then `Control pair-verify 1`,
`2`, `### Control pair-verify HK done: 1`. This is the ordering already implemented in
`src/carplay/net/mod.rs`.

The event channel is handled by `CarPlayEventHTTPClient.c` — named "Client" because the
device *connects out* to the accessory's `eventPort` and then serves requests on it. That
file holds `CarPlayHIDReceiver`, `hidSendReport`, `forceKeyFrame`, `changeModes`,
`requestUI`, `requestSiri`, `setNightMode`, alongside the literals `HTTP/1.1`,
`Content-Length` and `AirTunes/%s`. So the event channel very likely speaks **HTTP/1.1**
rather than RTSP/1.0. **[I]** — the version literal is present but could be used for
matching rather than emitting; confirm on the wire. The head unit's own control server
does answer `RTSP/1.0` (see `../ncm-probe/session.log`), so the RTSP codec is needed there
regardless.

`CarPlayHIDReceiver` runs at thread priority 90. **[V]**

### Initial SETUP — real iPhone request

```
features        : ["uiContext", "viewAreas", "cornerMasks", "focusTransfer"]
model           : "iPhone9,1"
deviceID        : "4c:74:bf:21:3f:0d"
name            : "测试2"
timingPort      : 50044              <- OUR NTP server port
sessionUUID     : "F5DD81DA-D177-4664-B180-D01CF7014FD4"
keepAliveLowPower : true
macAddress      : "4C:74:BF:21:3F:01"
osBuildVersion  : "18F72"
sourceVersion   : "545.22.21"
```

No `streams` key on the initial SETUP. `features` is not in the spec at all. **[V]**

Response: `{ eventPort : 59232, timingPort : 59029 }` — the accessory returns *its own*
timing port, which contradicts Table 30-45's description ("port the device is listening
on"); that looks like a copy-paste error in the spec. **[V]**

### Screen stream SETUP — real iPhone request

```
streams : [{
    timestampInfo      : [{name:"SubSu"},{name:"BePxT"},{name:"AfPxT"},
                          {name:"BefEn"},{name:"EmEnc"}]
    uuid               : "e5f7a68d-7b0f-4305-984b-974f677a150b"   <- accessory display uuid
    latencyMs          : 75          (absent on iOS 14.6, present on iOS 15.2)
    type               : 110
    streamConnectionID : 8998030715767377382   (negative on iOS 14.6)
}]
```

Response: `{ streams : [{ dataPort : 54149, type : 110 }] }`, sometimes with a
`timingPort` alongside. `timestampInfo` is undocumented — it is the
`supportsHighAccuracyTimestamps` feature. **[V]**

The `uuid` echoes the display's uuid from the accessory's `/info` `displays[]`. **[I]**

### Commands seen on the wire

`modesChanged` sent by the device, e.g.:

```
{ type: "modesChanged", params: {
    appStates: [ {entity:0, appStateID:2},
                 {entity:0, speechMode:-1, appStateID:1},
                 {entity:0, appStateID:3} ],
    resources: [ {entity:1, resourceID:1},      // 1 = main screen, 1 = controller
                 {entity:2, resourceID:2} ] } } // 2 = main audio, 2 = accessory
```

`hidSendReport` from the accessory:

```
{ type: "hidSendReport",
  uuid: "841507b5-1d0e-4468-9368-304e58fc7242",
  hidReport: 01 fa 02 bc 01 }        // 5 bytes
```

Decoding against the head unit's HID descriptor (report ID + X:u16 + Y:u16, logical max
800 / 480): `X = 0x02fa = 762`, `Y = 0x01bc = 444` → `0.95, 0.93` of an 800x480 screen,
exactly what the next log line prints. **[V]**


## 4. Bonjour TXT observed on real head units

```
VW 5GG035280D : deviceid=7E:11:59:C9:B3:7E features=0x44540380,0x61 flags=0x4
                model=5GG035280D pi=1a5a39dd-... srcvers=280.33.12
Kenwood 15CY  : deviceid=22:A0:B0:95:B6:54 features=0x4040280,0x41 fv=1.0.8
                flags=0x4 model=15CY MOP pi=6ed2779a-... srcvers=320.17
```

`flags=0x4` on both = "audio cable attached"; bit 1 ("not configured") clear.
`fv` appears on the Kenwood only, so treat it as optional. **[V]**

Device-side `_carplay-ctrl._tcp` advertises `id` (Bluetooth MAC) and `srcvers`;
`src/carplay/ctrl_server.rs` currently hardcodes `srcvers=67.0.0`, which is an
AirPlay-shaped value. Real device `sourceVersion` values are `545.22.21` (iOS 14.6) and
`600.8.1` (iOS 15.2).


## 5. Socket options seen in the logs

- screen stream sender: `SO_SNDBUF = 65536` **[V]**
- screen stream receiver: `SO_RCVBUF = 32768` and later `327680` **[V]**
- timing socket: `SO_TIMESTAMP` **[V]**
