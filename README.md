# carplayd

iPhone-side wired CarPlay implementation with pluggable video/audio/input providers targetting just a Raspberry Pi Zero 2 W (no MFi chip needed)

**This implements the iPhone side, meaning it streams video/audio/input to a head unit that it connects to.** It does not present a head unit to an iPhone. This allows forwarding of Android Auto or other custom UI.

## How it works
- the iPhone authenticates the head unit for MFi, so we can always accept whatever the head unit gives us
- handshake requires starting off as a USB gadget then role switching to USB host
    - /dev/mem is used to poke at the USB chip since regular roleswitch is broken
- carplayd handles video encode and input translation
    - providers written with a set of Rust traits
    - debug provider exposes carplayd over network

## Running
1. compile with `cargo`
2. install 80-carplay-ncm.rules if using NetworkManager
3. create a config folder and put a file serial_number in with the serial you want to expose
4. run, passing path to the config folder

## Tested configurations
Currently tested on a Raspberry Pi Zero 2 W with Raspberry Pi OS based on Debian 13 trixie, and a Toyota Prius Prime 2021 XLE head unit (1080x678 nonstandard resolution). Video, audio, and touchscreen were all working.

## Debug Android Auto bridge
There is an initial testing version of the Android Auto bridge using the network debug provider in `tools/aa-debug-bridge`. It builds and links with `opencardev/aasdk`.
