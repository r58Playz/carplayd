# Android Auto debug bridge

Headless USB Android Auto receiver using [opencardev/aasdk](https://github.com/opencardev/aasdk).
Run it on a Linux PC; it forwards video, input, metadata, and playback audio through
carplayd's debug network provider. The Pi decodes the complete AA H.264 canvas,
crops the advertised safe area, and encodes the retained pixels for CarPlay.

OpenAuto's service sequencing is adapted under GPL-3.0-or-later; see
[COPYING](COPYING). Neither sibling checkout is modified.

## aasdk provenance

The dependency comes from [opencardev/aasdk](https://github.com/opencardev/aasdk),
OpenCarDev's fork of the original [f1xpl/aasdk](https://github.com/f1xpl/aasdk) by
Michal Szwaj (f1x.studio). The verified checkout uses branch `newdev` at commit
[`9bf6adf933665dee26532201719fac14a047ccf1`](https://github.com/opencardev/aasdk/commit/9bf6adf933665dee26532201719fac14a047ccf1)
(2026-06-11, `CI update`), tagged `v4.1.20260427-newdev.5`.
Its [source headers](https://github.com/opencardev/aasdk/blob/9bf6adf933665dee26532201719fac14a047ccf1/include/aasdk/Messenger/Messenger.hpp)
specify GPL-3.0-or-later and retain the original author attribution.

CMake expects this dependency at `../aasdk`, beside the carplayd repository.
To reproduce it with a fresh checkout, run from the carplayd repository:

```sh
git clone --branch newdev https://github.com/opencardev/aasdk.git ../aasdk
git -C ../aasdk checkout --detach 9bf6adf933665dee26532201719fac14a047ccf1
```

## Build

Dependencies: C++17 compiler, CMake 3.16+, pkg-config, Boost development headers and system/log/log_setup
libraries, libusb 1.0 development files, OpenSSL development files, system Protobuf
and protoc of matching versions, and nlohmann-json development headers.

For Debian/Ubuntu:

```sh
sudo apt install build-essential cmake pkg-config libboost-all-dev libusb-1.0-0-dev \
    libssl-dev libprotobuf-dev protobuf-compiler nlohmann-json3-dev
```

From the carplayd repository:

```sh
cmake -S tools/aa-debug-bridge -B tools/aa-debug-build -DCMAKE_BUILD_TYPE=Release
cmake --build tools/aa-debug-build -j4
ctest --test-dir tools/aa-debug-build --output-on-failure
```

The build directory is ignored by Git.

Set `-DAASDK_SOURCE_DIR=/path/to/aasdk` for another clone. The build includes a
small compatibility facade for Boost releases that removed `io_service`, omits
aasdk’s unused legacy TCP implementation, and sets the compatibility policy needed by aasdk's older CMake helper. It uses the
system Protobuf instead of downloading build dependencies. No `cmake --install`
or installation of aasdk certificates is necessary.

`-DBUILD_AA_BRIDGE=OFF` builds the protocol/geometry/mixer tests without aasdk.
The sibling build automatically uses ccache when installed; set `CCACHE_DIR` to a
writable directory if building in a restricted environment.

## Run

Start carplayd on the Pi with the CarPlay head unit connected, then run:

```sh
tools/aa-debug-build/aa-debug-bridge pi-carplayd.lan
# or: aa-debug-bridge HOST --port 6767
```

Plug the Android phone into the PC's USB host port and accept Android Auto's
connection prompts. The user running the bridge needs USB access to the phone,
including its re-enumerated Google accessory device (`18d1:2d00` or `18d1:2d01`);
configure the host's normal udev USB permissions accordingly.

### Without the car

`tools/debug-receiver.py` stands in for carplayd on the same PC. It sends the init
and display request carplayd would send, shows the decoded AA canvas cropped to the
safe area, plays the PCM, and turns mouse, wheel and keys into input events:

```sh
python tools/debug-receiver.py                  # GTK window, listens on 127.0.0.1:6767
tools/aa-debug-build/aa-debug-bridge 127.0.0.1   # then plug in the phone
```

Messages carplayd would reject are logged with carplayd's reason. Stream failures
are counted as errors; fatal violations end the session. A crop that doesn't fit the
decoded frame is shown on the display. The receiver then waits for the next bridge
run. Useful options:

- `--size 1080x678 --fps 30`: the car's display (these are the defaults).
- `--knob`, `--touch touchpad|none`, `--input-devices FILE`: advertised controls.
- `--record DIR`: per session, the control trace (`control.jsonl`), each display's
  H.264 and per-frame timing, and each audio stream as WAV.
- `--headless --once`: no window; exit status 1 if the session reported errors.

Audio is played as carplayd would play it: 5 ms blocks paced by carplayd's drift
controller, starting (and resuming after an underrun) once 50 ms is queued. A
position discontinuity discards the queue, more than 400 ms queued drops the oldest
back to 50 ms, and an empty queue plays silence. These are counted per
stream (resets, stale drops, gaps) and logged once with carplayd's reason, so choppy
audio here means choppy audio in the car.

**Show full source and crop** outlines the safe area on the whole canvas. **Head
unit takes main audio** suspends main audio, as carplayd does for the car's own
sources. **Request display**, **Close display** and **Disconnect provider** test
restarts. Microphone input is not emulated.

Projection starts only after the debug display socket connects. The smallest AA
canvas containing the CarPlay screen is selected from 800×480, 1280×720, and
1920×1080. The CarPlay screen occupies the canvas's top-left corner; explicit AA UI
margins cover the right and bottom, and carplayd crops them away. AA lays its UI out
in that margin-adjusted display and does not subtract the margins from touch
positions, so the touchscreen and absolute touchpad are advertised at the CarPlay
size with positions relative to the CarPlay screen.
The bridge advertises 60 FPS when supported by the display, otherwise 30 FPS.

Audio channels:

- Media: signed 16-bit, 48 kHz stereo → CarPlay main media audio. AA sends the
  next packet only after an acknowledgment, so acknowledgments are released in real
  time, at most 50 ms ahead; acknowledging on receipt lets the phone outrun
  carplayd's live playback, which then drops the excess as stale.
- Guidance and system: signed 16-bit, 16 kHz mono → a shared alternate audio stream.
  Concurrent prompts are aligned by AA timestamp and mixed with saturation. Mixing
  starts 40 ms behind the first samples to absorb AA packet jitter, and up to 500 ms
  delivered faster than real time is kept rather than dropped.
- PCM positions stay contiguous unless an AA timestamp moves more than 40 ms from
  them. AA timestamps jitter and round to whole frames, and carplayd discards its
  buffered audio on every position discontinuity.
- Microphone: signed 16-bit, 16 kHz mono is advertised because AA ends the session
  without an audio input ("No audio/mic"). The car microphone is not forwarded yet;
  an opened microphone sends silence.
- Calls, Bluetooth, and wireless projection are not advertised.

Basic driving/night-mode sensors report a stationary test environment with daytime
mode. No location is synthesized. Media title, artist, album, duration, source,
playback status, repeat/shuffle, and position are forwarded when supplied by AA;
artwork is omitted because the debug protocol does not support it.

Every ten seconds stderr reports video rate, network queue bytes, dropped PCM
packets, and frame-position discontinuities. Network PCM queues retain at most about 60 ms;
blocks are at most 20 ms. Video acknowledgments wait for the corresponding network
write, with a bounded queue and a write timeout. No desktop audio device is opened.

USB unplug stops audio and clears metadata/input state while keeping the debug
display connection. Replugging creates a fresh AA session and rebases video time
onto the host's continuous clock. Debug-network loss exits the tool; restart it
with a fresh carplayd session. Ctrl+C stops the current session.

## Validate on hardware

1. Exercise cropping before connecting a phone. For a 1080×678 CarPlay display:

   ```sh
   python tools/debug-network-client.py pi-carplayd.lan --test-source \
       --source-size 1280x720 --safe-area 100,20,1080,678
   ```

   The local encoded preview includes green margins. The CarPlay output must
   remove them exactly, without resizing or extending edges.
2. Run the AA bridge. Confirm that UI margins keep all controls inside the crop;
   test touches in each corner, touchpad releases, rotary navigation, and media keys.
   AA UI-margin support must be confirmed on the actual phone/version.
3. Play media for ten minutes; overlap navigation/system sounds and check prompt
   tails, pause/resume, head-unit audio ownership, and return to media.
4. Unplug/replug USB repeatedly. Check metadata clearing, audio release, recovery
   with fresh headers/keyframes, and absence of a video timestamp jump.
5. Record stderr counters alongside Pi CPU/temperature during concurrent video and
   audio. Protocol tests do not establish actual phone/head-unit compatibility.

The CTest fake-provider test exercises fragmented control reads, display response
ordering, video/PCM binary framing, metadata, audio status, and stream shutdown.
Rust provider tests cover interleaved control traffic while either display
handshake stage is pending, timeout/cancellation, and strict crop validation.
