# mimint-esp32-v2 — firmware skeleton (Phase 7)

Reference firmware for the Mimint v2 device path: an ESP32 that pairs
with the Mimint hub using the pinned P-256 identity key + ECDH
ephemeral + AES-256-GCM session (the same friends protocol as
desktop bodies), then exchanges text turns with the hub's brain.

## Build status: BLOCKED — toolchain absent

**This firmware has NOT been compiled.** No ESP-IDF toolchain is
installed on the build machine (vega): `idf.py` is not on `PATH`,
there is no `~/esp-idf` checkout and no `~/.espressif` directory
(checked 2026-11-19). Nothing in this directory claims a successful
build, and no firmware binary exists.

## Exact install steps (to unblock)

On a machine with network access (Debian/Ubuntu; adapt the package
manager for other distros):

```sh
# 1. Prerequisites (no sudo needed if you install under $HOME).
sudo apt-get install git wget flex bison gperf python3 python3-pip \
  python3-venv cmake ninja-build ccache libffi-dev libssl-dev dfu-util \
  libusb-1.0-0

# 2. Fetch ESP-IDF (v5.3 is the pinned release for this skeleton).
git clone -b v5.3 --recursive https://github.com/espressif/esp-idf.git ~/esp-idf

# 3. Install the toolchain into ~/.espressif (user-local).
~/esp-idf/install.sh esp32

# 4. Enter the environment and build.
. ~/esp-idf/export.sh
idf.py --version            # must print ESP-IDF v5.3.x
cd firmware/mimint-esp32-v2
idf.py set-target esp32
idf.py build
```

Then flash with `idf.py -p /dev/ttyUSB0 flash monitor`. The device
provisions its P-256 identity key at first boot (NVS plaintext in v1
— known limit M5; NVS encryption is future work, §13 of the design).

## What this skeleton contains

- `CMakeLists.txt` — project file (`idf_component_register` for `main`).
- `main/main.c` — skeleton: NVS init, identity-key provisioning
  stub, Wi-Fi station stub, and the friends-session handshake stub.
  It does NOT open any control server: there is deliberately no
  HTTP control endpoint on any port, and no WS server on the device.
  The device is a WS *client* to the hub only.
- `sdkconfig.defaults` — minimal defaults (ESP32, no Bluetooth).

## What is deliberately absent

- No HTTP server, no `:8080` control server (the fork board's old WS
  control server was removed — see `esp32_no_8080_in_fork`).
- No `reboot` / `upgrade_firmware` handling: the hub never calls
  them, and this firmware does not implement them.
- No MQTT, no UDP audio, no firmware-over-OTA: the only transport
  is the WebSocket to the hub (WSS, or `ws://` loopback/tailnet in
  lab mode only).
