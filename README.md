# carplayd

iPhone-side wired CarPlay implementation with pluggable video/audio/input providers targetting just a RPi Zero 2 W (no MFi chip needed)

**This implements the iPhone side, meaning it streams video/audio/input to a head unit that it connects to.** It does not present a head unit to an iPhone. This allows forwarding of Android Auto or other custom UI.

## How It Works
- the iPhone authenticates the head unit for MFi, so we can always accept whatever the head unit gives us
- handshake requires starting off as a USB gadget then role switching to USB host
    - /dev/mem is used to poke at the USB chip since regular roleswitch is broken
- carplayd handles video encode and input translation
    - providers written with a set of Rust traits
    - debug provider exposes carplayd over network

## Running
1. compile
2. install 80-carplay-ncm.rules if using NetworkManager
3. create a config folder and put a file serial_number in with the serial you want to expose
4. run, passing path to the config folder

## Debug Android Auto bridge

There is an initial testing version of the Android Auto bridge using the network debug provider in `tools/aa-debug-bridge`.

The [desktop Android Auto bridge](tools/aa-debug-bridge/README.md) uses
[opencardev/aasdk](https://github.com/opencardev/aasdk), OpenCarDev's fork of the
original [f1xpl/aasdk](https://github.com/f1xpl/aasdk) by Michal Szwaj (f1x.studio).
The bridge was built and tested against branch `newdev` at commit
[`9bf6adf933665dee26532201719fac14a047ccf1`](https://github.com/opencardev/aasdk/commit/9bf6adf933665dee26532201719fac14a047ccf1)
(2026-06-11). See the bridge README for dependency checkout and build instructions.
