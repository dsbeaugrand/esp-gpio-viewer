# On-Device End-to-End Verification (task `esp-gpio-viewer-96w`)

This runbook verifies the `esp-gpio-viewer` crate on **real hardware**. Everything up to
this point is proven only to the build boundary (both example firmwares link, all protocol
bytes are host-tested against the C++ reference). This step confirms the firmware actually
boots, joins WiFi, serves the hosted UI, and streams live pin data.

**You run these steps** (Claude cannot access your ESP32, USB port, WiFi, or browser).
Report the checklist at the bottom back to Claude, who will help debug any failures.

---

## 0. Prerequisites

- An **ESP32** (WROOM/WROVER) or **ESP32-S3** dev board on USB.
- `espflash` installed (already present at `~/.cargo/bin/espflash`).
- The `esp` Rust toolchain (already installed).
- Your 2.4 GHz WiFi credentials. **STA mode only** — the firmware does not do AP mode.
- The board and your computer on the **same network** (so the browser can reach the device IP).
- `curl` (for the protocol spot-checks).

> Wire references for the example's registered pins (below) are optional but make the
> live-update checks visible. GPIO2 is the onboard LED on most ESP32 dev boards; GPIO0 is
> the BOOT button.

---

## 1. Flash the firmware

Set WiFi creds and flash. `cargo +esp run` builds, flashes, and opens the serial monitor.

### ESP32 (classic — the board being verified here)

> **Two boards are connected.** This machine has BOTH a classic ESP32 and an ESP32-S3
> plugged in. Verified port assignment (via `espflash board-info`):
> - `/dev/cu.usbserial-0001` → **esp32 (rev v3.1)** ← flash THIS one
> - `/dev/cu.wchusbserial210` → esp32s3 (rev v0.2) ← do NOT use
>
> The `ESPFLASH_PORT` below pins the classic ESP32 so `espflash` cannot pick the S3.

```bash
cd ~/Desktop/Development/repos/esp-gpio-viewer
source ~/.cargo/env
source ~/export-esp.sh                 # puts the xtensa-esp32-elf-gcc linker on PATH
export WIFI_SSID="your-ssid"
export WIFI_PASSWORD="your-password"
export ESPFLASH_PORT=/dev/cu.usbserial-0001   # <-- classic ESP32, NOT the S3
cargo +esp run --release -Zbuild-std=core,alloc \
  --target xtensa-esp32-none-elf \
  --example esp32 --features esp32,server
```

### ESP32-S3
```bash
cd ~/Desktop/Development/repos/esp-gpio-viewer
source ~/.cargo/env && source ~/export-esp.sh
export WIFI_SSID="your-ssid" WIFI_PASSWORD="your-password"
cargo +esp run --release -Zbuild-std=core,alloc \
  --target xtensa-esp32s3-none-elf \
  --example esp32s3 --features esp32s3,server
```

If flashing needs an explicit port: `espflash flash --monitor --chip esp32 <path-to-elf>`,
or set `ESPFLASH_PORT=/dev/tty.usbserial-XXXX`.

**Expected serial output** (the example prints these):
```
esp32: embassy + esp-rtos started
esp32: waiting for Wi-Fi + DHCP...
wifi: connected to <your-ssid>
esp32: online at http://<DEVICE_IP>:8080/
```

✅ **Checkpoint 1 — boot + WiFi + DHCP:** you see `online at http://<ip>:8080/`.
Note the `<DEVICE_IP>` — you need it below.

---

## 2. Browser check — the headline acceptance

Open **`http://<DEVICE_IP>:8080/`** in a desktop browser (Chrome/Firefox).

The device serves a tiny HTML shim whose `<base href>` points at the hosted Vue UI
(`thelastoutpostworkshop.github.io/.../gpio_viewer_1_5/`), so **the browser must have
internet access** to load the UI assets. The device only serves data.

✅ **Checkpoint 2 — UI renders:** the GPIOViewer board view appears and pin tiles for the
registered pins show up (no infinite spinner). If tiles never render, see *Troubleshooting*.

### Registered pins in the example
| GPIO | Type | Example wiring | Expected tile |
|------|------|----------------|---------------|
| **2** | Digital output | onboard LED | toggles with the LED |
| **0** | Digital input | BOOT button | 1 idle, 0 while pressed (or vice-versa) |
| **4** | PWM (LEDC ch0, 13-bit) | LED + resistor, or scope | 0–255 as duty changes |
| **34** (esp32) / **1** (s3) | Analog (ADC) | — | **reads 0** (see note) |

> **Analog note:** the example's `analog_source` is a documented placeholder returning `0`
> (a `fn` pointer can't capture the `Adc` driver). So the ADC tile will read 0 until you
> supply a real reader. This is expected, not a failure. To see live analog, replace
> `read_adc` in the example with a real one-shot read from a firmware-owned `Adc`.

✅ **Checkpoint 3 — live update:** press/release the **BOOT button (GPIO0)** and watch its
tile flip within ~100 ms (the sampling interval). This proves the sampler → SSE → UI path.

---

## 3. Protocol spot-checks with `curl`

Run these from any machine on the same network. They validate the REST bytes match the
contract reproduced from the C++ reference.

```bash
IP=<DEVICE_IP>

curl -s http://$IP:8080/release       # -> {"release": "1.7.1"}
curl -s http://$IP:8080/sampling      # -> {"sampling": "100"}
curl -s http://$IP:8080/free_psram    # -> {"sampling": "100 No PSRAM"} on ESP32 (no PSRAM)
curl -s http://$IP:8080/pinmodes      # -> [{"pin":"2","mode":"3"},{"pin":"0","mode":"1"}, ...]
curl -s http://$IP:8080/pinfunctions  # -> {"boardpinsfunction":[{"name":"ADC","functions":[...]},{"name":"Touch",...}]}
curl -s http://$IP:8080/espinfo       # -> {"chip_model":"ESP32", ... } real chip/flash/heap fields
curl -s http://$IP:8080/partition     # -> [{"label":"nvs","type":1,...}, ...]
```

✅ **Checkpoint 4 — REST bytes:** each response matches the shape shown. `/espinfo` should
show **real** values now (chip revision, CPU freq, MAC, reset reason, uptime advancing
between calls).

### SSE stream
```bash
curl -N http://$IP:8080/events
```
Expected: an immediate **baseline** `gpio-state` event listing all registered pins, then
`free_heap` events, periodic heartbeats, and a fresh `gpio-state` whenever a pin changes.
Frames look like:
```
event: gpio-state
data: {"2": {"s": 0, "v": 0, "t": 0}, "0": {"s": 256, "v": 1, "t": 0}, ...}

event: free_heap
data: 210.34 KB
```
Toggle GPIO0 while `curl -N` runs → a new `gpio-state` frame appears. Ctrl-C to stop.

✅ **Checkpoint 5 — SSE:** baseline arrives on connect; a pin change emits a `gpio-state`
frame; `free_heap` shows a real, non-zero value (proves `free_heap_source` injection works).

> **ESP32 single-worker caveat (by design):** the ESP32 example runs `WEB_TASK_POOL_SIZE=1`
> (DRAM-bound — pool=2 won't link). With `KeepAlive::Close`, REST calls close promptly so the
> lone worker cycles between REST and SSE. If you hold `curl -N /events` open **and** the
> browser UI is also connected, one of them may wait — that's the known DRAM tradeoff. The
> **ESP32-S3** example uses pool=2 and has no such limit. Test SSE and browser separately on
> plain ESP32.

---

## Verified results (classic ESP32 rev v3.1, 2026-07-09)

Programmatically confirmed on device `192.168.50.48:8080` (hand-rolled HTTP/SSE server,
release build):
- ✅ **Checkpoint 1** — boots, joins `<your-ssid>`, DHCP, prints `online at ...`
- ✅ **Checkpoint 4** — REST 7/7 `200`, bytes match the C++ contract; `/espinfo` real
  (ESP32, 2 cores, rev 301, 240 MHz, real MAC); `/partition` real (nvs/otadata/app0)
- ✅ **Checkpoint 5** — SSE baseline `gpio-state` (4 pins, correct `s/v/t`) + live
  `free_heap` (~83 KB real value); heartbeat working
- ✅ **Concurrency** — 5/5 REST succeed while SSE held open (multi-socket, no contention)
- ⏳ **Checkpoints 2 & 3** — browser UI render + live GPIO0 button toggle: **user to confirm**

> History: picoserve 0.18 null-derefs in its serve future on esp-rtos 0.3 (8 rounds of
> hardware debugging confirmed it across every core/profile combo). Replaced with a
> hand-rolled HTTP/SSE server (`src/server.rs` + `src/http.rs`), which fixed it and
> raised the main stack 24 KB → 88 KB. Architect-reviewed (APPROVED) + graceful
> drain-on-close hardening (M1) applied.

## 4. Report back — results checklist

Copy this, fill in, and send to Claude:

```
Board: [ESP32 | ESP32-S3]
1. Boot + WiFi + DHCP (serial "online at ..."):   [PASS | FAIL — paste serial]
2. Browser UI renders + tiles appear:             [PASS | FAIL — what showed]
3. GPIO0 button flips its tile live:              [PASS | FAIL]
4. curl REST endpoints match shapes:              [PASS | FAIL — paste any mismatch]
5. curl -N /events baseline + change + free_heap: [PASS | FAIL — paste a few frames]
Notes / anything odd:
```

---

## Troubleshooting

- **Serial stuck at "waiting for Wi-Fi + DHCP":** wrong creds, 5 GHz-only SSID (ESP32 is
  2.4 GHz only), or weak signal. Re-check `WIFI_SSID`/`WIFI_PASSWORD`.
- **UI spinner forever, but `/release` curl works:** the browser can't reach the hosted Vue
  assets (no internet / corporate proxy / content blocker). The device is fine.
- **On plain ESP32, UI won't init while `curl -N /events` is open:** expected single-worker
  behavior (see caveat). Close the curl stream, reload the UI.
- **Link error `xtensa-esp32-elf-gcc not found`:** you didn't `source ~/export-esp.sh` in
  this shell.
- **`espflash` can't find the port:** set `ESPFLASH_PORT=/dev/tty.usbserial-XXXX` (macOS:
  `ls /dev/tty.usb*`).
- **`/espinfo` reset_reason after a fresh USB flash:** typically a software/power-on reset
  code — that's normal.
```
