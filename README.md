# carplayd

iPhone-side CarPlay implementation and bridge to Android Auto (wip) targetting RPi Zero 2 W

## cross-compiling and copying to the Pi

Run `./build-pi.sh` to build a release binary for `aarch64-unknown-linux-gnu` and
copy it to `user@pi-carplayd.lan:~`. Use `./build-pi.sh --build-only` to skip the
copy, or pass another destination: `./build-pi.sh user@another-pi:~`.

The script uses `aarch64-linux-gnu-gcc` and its sysroot, selecting the target's
Linux headers for `v4l2r` binding generation. Host libclang is required for
bindgen; no target libclang or libv4l2 library is needed. For another AArch64 glibc
toolchain, including Buildroot, set `CARPLAYD_LINKER` to its compiler executable.
Set `CARPLAYD_SYSROOT` if you need to override the compiler's reported sysroot.

## running
1. compile
2. install 80-carplay-ncm.rules if using NetworkManager
3. create a config folder and put a file serial_number in with the serial you want to expose
4. run, passing path to the config folder
