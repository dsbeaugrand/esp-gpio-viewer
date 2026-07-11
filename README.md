# esp-gpio-viewer

A `no_std` ESP32 Rust port of the [GPIOViewer](https://github.com/thelastoutpostworkshop/gpio_viewer)
backend. The upstream C++ library serves an HTTP + Server-Sent-Events stream that feeds
a remotely hosted Vue UI; because that UI is hosted remotely, the only surface this crate
ports is the backend HTTP/SSE protocol.

> **Using this in your own ESP32 project?** See **[docs/INTEGRATION.md](docs/INTEGRATION.md)** —
> a step-by-step guide to add live GPIO/PWM/ADC monitoring to an existing esp-hal + embassy firmware.

## Layout

- `src/protocol.rs` — byte-exact JSON / HTML / SSE serializers (pure, host-testable).
- `src/value.rs` — `readGPIO` value/type mapping (pure, host-testable).
- `src/lib.rs` — `GpioViewer` / `GpioViewerBuilder` / `PinMode` / `PinType`.
- `src/sampler.rs` — the embassy sampler + broadcast channel (`server`).
- `src/server.rs` — hand-rolled async HTTP/1.1 + SSE server over embassy-net (`ServerState`, `accept_loop`) (`server`).
- `src/http.rs` — pure HTTP request-line parse + response-header builder (host-testable) (`server`).
- `src/hwinfo.rs` — real chip/flash/heap reads for `/espinfo` + the partition-table reader (`server`).
- `examples/esp32.rs`, `examples/esp32s3.rs` — complete per-chip firmware (Wi-Fi STA + serve).

## Building

The crate is `no_std` for embedded builds and links `std` only under `cfg(test)`, so the
pure-logic test suite runs on the host.

### Host tests

```bash
cargo test
```

`.cargo/config.toml` sets no default `[build] target`, so plain `cargo test` builds for the
host target and never cross-compiles — even though `rust-toolchain.toml` selects the `esp`
toolchain (which ships the host `std`).

### Cross-compile the library for a chip (Xtensa, `esp` toolchain)

```bash
# ESP32
cargo +esp build --release -Zbuild-std=core,alloc --target xtensa-esp32-none-elf   --features esp32,server

# ESP32-S3
cargo +esp build --release -Zbuild-std=core,alloc --target xtensa-esp32s3-none-elf --features esp32s3,server
```

## Runnable examples (per chip)

`examples/esp32.rs` and `examples/esp32s3.rs` are complete firmware: they bring up Wi-Fi
**STA** + DHCPv4 (esp-radio 0.18 + embassy-net), serve the GPIOViewer HTTP/SSE API with
picoserve, and run the sampler. Point the hosted GPIOViewer UI (or a browser) at
`http://<ip>:8080/` — the device logs its URL on boot.

### 1. Set Wi-Fi credentials (compile-time, never committed)

The examples read `WIFI_SSID` / `WIFI_PASSWORD` via `env!` at **compile time**; no secret is
hard-coded or checked in. Export them (a `_secrets`-style local file you keep out of git, e.g.
`source ./_secrets.sh`, is the recommended pattern):

```bash
# _secrets.sh (git-ignored)
export WIFI_SSID="your-network"
export WIFI_PASSWORD="your-password"
```

### 2. Build

```bash
# ESP32
cargo +esp build --release -Zbuild-std=core,alloc --target xtensa-esp32-none-elf \
    --example esp32 --features esp32,server,example-esp32

# ESP32-S3
cargo +esp build --release -Zbuild-std=core,alloc --target xtensa-esp32s3-none-elf \
    --example esp32s3 --features esp32s3,server,example-esp32s3
```

### 3. Flash + monitor (espflash)

```bash
cargo +esp run --release -Zbuild-std=core,alloc --target xtensa-esp32-none-elf \
    --example esp32 --features esp32,server,example-esp32
```

> Linking requires the Xtensa GCC toolchain on `PATH` (installed by `espup`); `source
> ~/export-esp.sh` first if `xtensa-esp32-elf-gcc` is not found.

### Wiring your own hardware

The builder injects the firmware's peripherals through allocator-/driver-agnostic seams:

```rust
let viewer = GpioViewer::builder()
    .port(8080)
    .register(2, PinMode::Output, PinType::Digital)  // digital output
    .register(0, PinMode::Input,  PinType::Digital)  // digital input
    .pwm(4, 0, 13)                                   // LEDC channel 0, 13-bit
    .adc(34)                                         // ADC pin
    .free_heap_source(|| esp_alloc::HEAP.free() as u32)
    .free_psram_source(|| esp_alloc::psram_free() as u32) // omit on boards without PSRAM
    .analog_source(read_adc)                         // fn(u8) -> u16
    .build();
```

To serve the board's **real** partition table instead of the representative
`server::DEFAULT_PARTITIONS`, read it once at boot and inject the slice:

```rust
let mut flash = /* an embedded_storage::Storage, e.g. esp_storage::FlashStorage */;
let mut buffer = [0u8; 0xC00];
let mut parts: heapless::Vec<PartitionInfo, 16> = heapless::Vec::new();
esp_gpio_viewer::hwinfo::read_partition_infos(&mut flash, &mut buffer, &mut parts)?;
// pass `parts.as_slice()` (behind a `'static`) to `AppState::new(..)`
```

## Features

| Feature   | Effect                                                                        |
|-----------|-------------------------------------------------------------------------------|
| _(none)_  | Pure `no_std` library: only `heapless`. Host-testable.                        |
| `server`  | picoserve router + sampler + async network stack + example runtime bits.      |
| `esp32`   | esp-hal ESP32 chip + Wi-Fi/RTOS/alloc/backtrace/println for the example.       |
| `esp32s3` | esp-hal ESP32-S3 chip + the same runtime stack.                               |

## REST + SSE routes (feature `server`)

`GET` unless noted. `/` returns `text/html`; `/events` is `text/event-stream`; the rest JSON.

| Route            | Body                                                                    |
|------------------|-------------------------------------------------------------------------|
| `/`              | Index HTML that boots the hosted Vue UI.                                |
| `/release`       | `{"release": "<ver>"}`                                                   |
| `/sampling`      | `{"sampling": "<ms>"}`                                                   |
| `/free_psram`    | Free PSRAM from the injected `free_psram_source` (or `No PSRAM`).       |
| `/espinfo`       | Real chip / flash / heap info (`hwinfo`; honest fallbacks where no API).|
| `/partition`     | The injected flash partition array (fallback `DEFAULT_PARTITIONS`).     |
| `/pinmodes`      | Registered-pin mode array.                                             |
| `/pinfunctions`  | Per-chip ADC / Touch board-pin-function object.                        |
| `/events`        | Server-Sent-Events stream of live `gpio-state` / `free_heap` frames.   |

### `/espinfo` data provenance

Values are live esp-hal reads where a clean `no_std` API exists (CPU freq, chip revision,
MAC, reset reason, uptime) plus the injected heap/PSRAM figures; fields with no `no_std`
source (flash mode/size/speed, total heap, Arduino sketch info) report honest `"n/a"`/`0`/
`None`. See the table in `src/hwinfo.rs` for the per-field breakdown.
