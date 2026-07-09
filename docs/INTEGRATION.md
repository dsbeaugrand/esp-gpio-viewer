# Using `esp-gpio-viewer` in your own ESP32 Rust project

This guide shows how to drop `esp-gpio-viewer` into an existing `no_std` ESP32 Rust firmware
so you can watch that project's GPIO / PWM / ADC pins live in a browser — the same
[GPIOViewer](https://github.com/thelastoutpostworkshop/gpio_viewer) web UI, served by your
device. You register the pins you care about, spawn two kinds of background task, and open
`http://<device-ip>:8080/`.

Pin reads are **non-invasive**: the crate reads the GPIO/LEDC hardware registers directly, so
it observes pins your application already owns and drives — you do **not** hand pin ownership
to the viewer.

---

## 1. Prerequisites — your project must already be on this stack

`esp-gpio-viewer` is built on `esp-hal` + `embassy` + `esp-rtos`, and its versions must match
your project's. It is verified against:

| Crate | Version |
|---|---|
| `esp-hal` | `1.1` (with `unstable`) |
| `esp-rtos` | `0.3` (`embassy` feature) |
| `esp-radio` | `0.18` (Wi-Fi STA) |
| `embassy-net` | `0.9` (dhcpv4, tcp) |
| `embassy-time` | `0.5` |
| `embassy-sync` | `0.8` |
| `embassy-executor` | `0.10` |
| `esp-alloc` | `0.10` |
| Rust toolchain | `esp` channel |

Your firmware must already: init `esp-hal`, set up `esp-rtos` + an embassy executor, bring up
**Wi-Fi STA + an `embassy-net` stack with an IP** (STA only — AP mode is unsupported), and have
a global allocator (`esp-alloc`). If you don't have Wi-Fi/embassy-net yet, copy the bootstrap
verbatim from [`examples/esp32.rs`](../examples/esp32.rs).

> **Heads-up on features:** the `esp32` / `esp32s3` feature currently also pulls the
> example-runtime crates (`esp-radio`, `esp-rtos`, `esp-alloc`, `esp-backtrace`, `esp-println`).
> If your project already depends on those, keep the versions in the table above aligned to
> avoid a duplicate/incompatible-version build error. (A leaner `examples`-only feature split is
> a planned improvement.)

---

## 2. Add the dependency

The crate isn't on crates.io yet — depend on it by git or path. Enable your **chip** feature
plus **`server`**:

```toml
# Cargo.toml
[dependencies]
esp-gpio-viewer = { git = "https://github.com/dsbeaugrand/esp-gpio-viewer", features = ["esp32", "server"] }
# or a local checkout:
# esp-gpio-viewer = { path = "../esp-gpio-viewer", features = ["esp32", "server"] }
```

Use `"esp32s3"` instead of `"esp32"` for an ESP32-S3. Build in **release** (`--release`) —
debug stack frames are too large for the classic ESP32.

### DRAM requirement (classic ESP32)
esp-radio's Wi-Fi driver needs its DMA buffers in real internal DMA-capable SRAM. Make sure
your heap has **≥ 32 KB in normal `dram_seg`** (16 KB faults the first TCP connection). The
example uses:

```rust
esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 96 * 1024); // bulk (reclaimed RAM)
esp_alloc::heap_allocator!(size: 32 * 1024);                            // DMA-capable — required
```

---

## 3. Wire it up (5 steps)

Add these alongside your existing tasks, after your `embassy-net` `stack` has an IP.

```rust
use esp_gpio_viewer::{GpioViewer, PinMode, PinType};
use esp_gpio_viewer::sampler::{run_sampler, FrameChannel};
use esp_gpio_viewer::server::{accept_loop, ServerState, DEFAULT_PARTITIONS, DEFAULT_PORT};
use embassy_net::Stack;
use static_cell::StaticCell;

// -- (1) A single broadcast channel the sampler publishes frames to, SSE clients subscribe to.
static EVENTS: FrameChannel = FrameChannel::new();

// -- (2) Return a real ADC reading for analog pins. A plain `fn` pointer can't capture your
//        `Adc` driver, so expose the read via a free function / global (or return 0 to stub).
fn read_adc(_gpio: u8) -> u16 { 0 } // replace with a real one-shot read

// -- (3) The sampler task: reads registered pins every 100 ms, streams diffs + free-heap.
#[embassy_executor::task]
async fn viewer_sampler(viewer: &'static GpioViewer) -> ! {
    run_sampler(&EVENTS, viewer).await
}

// -- (4) The web worker: one TCP socket serving REST + SSE. Spawn several for concurrency.
const WEB_POOL_SIZE: usize = 3;
#[embassy_executor::task(pool_size = WEB_POOL_SIZE)]
async fn viewer_web(stack: Stack<'static>, state: &'static ServerState) -> ! {
    let mut rx = [0u8; 1536];
    let mut tx = [0u8; 2048];
    accept_loop(stack, DEFAULT_PORT, state, &mut rx, &mut tx).await
}

// -- (5) In your main, after Wi-Fi/DHCP is up and you know `stack` + your IP string:
fn spawn_viewer(
    spawner: embassy_executor::Spawner,
    stack: Stack<'static>,
    ip_str: &str, // e.g. "192.168.1.42", from stack.config_v4()
) {
    // Register the pins you want to watch. These are the SAME pins your app drives.
    let viewer = GpioViewer::builder()
        .port(DEFAULT_PORT)                              // 8080
        .register(2, PinMode::Output, PinType::Digital)  // a digital output you toggle
        .register(0, PinMode::Input,  PinType::Digital)  // a digital input
        .pwm(4, /*ledc channel*/ 0, /*resolution bits*/ 13) // a PWM/LEDC pin
        .adc(34)                                         // an ADC pin
        .free_heap_source(|| esp_alloc::HEAP.free() as u32) // real free-heap readout
        .analog_source(read_adc)                         // real ADC readings
        .build();

    static VIEWER: StaticCell<GpioViewer> = StaticCell::new();
    let viewer: &'static GpioViewer = VIEWER.init(viewer);

    static STATE: StaticCell<ServerState> = StaticCell::new();
    let state: &'static ServerState = STATE.init(ServerState::new(
        viewer,
        ip_str,
        "n/a",               // free-sketch-RAM string shown in the UI header (optional)
        &EVENTS,
        DEFAULT_PARTITIONS,  // or inject your real flash table (see §5)
    ));

    spawner.spawn(viewer_sampler(viewer)).unwrap();
    for _ in 0..WEB_POOL_SIZE {
        spawner.spawn(viewer_web(stack, state)).unwrap();
    }
}
```

**Socket budget:** the web workers each take a TCP socket, so size your `embassy-net`
`StackResources<N>` to at least `WEB_POOL_SIZE + 1` (workers + DHCP). Then open
`http://<device-ip>:8080/` in a browser.

---

## 4. Registering pins

`.register(pin, mode, pin_type)`, plus the convenience methods `.pwm(...)` and `.adc(...)`:

- **Digital** — `.register(pin, PinMode::{Input|Output|InputPullup|InputPulldown|OutputOpenDrain}, PinType::Digital)`. Read live from the GPIO input register, non-invasively. `mode` is reported to the UI (via `/pinmodes`); it does not reconfigure the pin.
- **PWM** — `.pwm(pin, channel, resolution_bits)`. Reads the LEDC channel's live duty register (non-invasive) and scales it 0–255. Pass the LEDC **channel** and **resolution** you configured that pin with.
- **Analog** — `.adc(pin)` + `.analog_source(fn(u8) -> u16)`. Unlike GPIO/LEDC, the ADC can't be read from a register non-invasively (it needs a live `Adc` driver), so the reading comes from your injected function. Without one it reports `0`.

Up to `MAX_REGISTERED_PINS` (48) pins. Sampling interval is 100 ms (`DEFAULT_SAMPLING_INTERVAL_MS`).

---

## 5. Optional: real system info

The library stays allocator/peripheral-agnostic — you inject the readings:

- `.free_heap_source(|| esp_alloc::HEAP.free() as u32)` — live free-heap in the `/free_heap` SSE frame.
- `.free_psram_source(|| /* your psram-free bytes */)` — enables the `/free_psram` value (omit → "No PSRAM").
- **Partitions** — `ServerState::new(..., DEFAULT_PARTITIONS)` serves a typical layout. For your board's real table, read it via `esp-bootloader-esp-idf` into a `&'static [PartitionInfo]` (see `read_partition_infos` in `src/hwinfo.rs`).
- `/espinfo` chip/flash/reset-reason/uptime data is read from `esp-hal` automatically.

---

## 6. What the browser gets

The device serves a tiny HTML shim that loads the **remotely hosted** Vue UI, so the browser
needs internet access (the device only serves data). Endpoints on port 8080:
`/`, `/release`, `/sampling`, `/free_psram`, `/pinmodes`, `/pinfunctions`, `/espinfo`,
`/partition`, and the SSE stream `/events` (events `gpio-state`, `free_heap`, `free_psram`).

Verify with curl:
```bash
curl -s http://<device-ip>:8080/pinmodes
curl -N http://<device-ip>:8080/events        # live stream; toggle a pin to see gpio-state frames
```

---

## 7. Gotchas

- **Release builds only** — debug stack frames overflow on the classic ESP32.
- **32 KB DMA heap** in `dram_seg` is mandatory for Wi-Fi (see §2).
- **Wi-Fi STA only** — no AP mode.
- **Analog reads are 0** until you wire a real `analog_source`.
- **Browser needs internet** to fetch the hosted UI assets; a spinner with working `curl`
  usually means blocked asset access, not a device problem.
- Full standalone reference firmware: [`examples/esp32.rs`](../examples/esp32.rs) /
  [`examples/esp32s3.rs`](../examples/esp32s3.rs). On-device verification steps:
  [`docs/ON_DEVICE_VERIFICATION.md`](./ON_DEVICE_VERIFICATION.md).
