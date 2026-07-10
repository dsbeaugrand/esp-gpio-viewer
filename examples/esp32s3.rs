//! Runnable **ESP32-S3** firmware for `esp-gpio-viewer`.
//!
//! Mirrors `examples/esp32.rs` for the S3: Wi-Fi **STA** + DHCPv4 (esp-radio 0.17 +
//! embassy-net), the GPIOViewer HTTP/SSE API served by the crate's hand-rolled server, and the
//! sampler broadcasting live pin state. Point the hosted GPIOViewer UI (or a browser) at
//! `http://<ip>:8080/`.
//!
//! ## Build & flash
//! ```bash
//! export WIFI_SSID="your-ssid"
//! export WIFI_PASSWORD="your-password"
//!
//! cargo +esp build --release -Zbuild-std=core,alloc --target xtensa-esp32s3-none-elf \
//!     --example esp32s3 --features esp32s3,server
//!
//! cargo +esp run --release -Zbuild-std=core,alloc --target xtensa-esp32s3-none-elf \
//!     --example esp32s3 --features esp32s3,server
//! ```
//! See the project README for the `_secrets`-style note on setting the two env vars.

#![no_std]
#![no_main]

use core::fmt::Write as _;

use alloc::string::String;
use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
// esp-radio 0.17 STA shape: `wifi::new` yields a `WifiController` + `Interfaces`; the STA netif is
// a `WifiDevice`. Credentials go through `ModeConfig::Client(ClientConfig)`, and a dropped link is
// observed via `wait_for_event(WifiEvent::StaDisconnected)`.
use esp_radio::wifi::{
    ClientConfig, Config as WifiConfig, ModeConfig, WifiController, WifiDevice, WifiEvent,
};
use static_cell::StaticCell;

use esp_gpio_viewer::sampler::{run_sampler, FrameChannel};
use esp_gpio_viewer::server::{accept_loop, ServerState, DEFAULT_PARTITIONS};
use esp_gpio_viewer::{GpioViewer, PinMode, PinType, DEFAULT_PORT};

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

/// Wi-Fi credentials injected at **compile time** (never hard-coded / committed).
const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

/// Number of concurrent HTTP/SSE worker tasks. The hand-rolled server has a tiny per-socket cost
/// (a couple of KB of buffers, a shallow poll — no 97 KB picoserve future), so several run: one can
/// hold the long-lived `/events` SSE stream while the others handle the UI's parallel REST fetches.
const WEB_POOL_SIZE: usize = 3;

/// DHCP client socket + one TCP socket per web worker.
const STACK_SOCKET_COUNT: usize = WEB_POOL_SIZE + 1;

/// The sampler's broadcast channel (`const`-constructible `static`).
static EVENTS: FrameChannel = FrameChannel::new();

/// Firmware-injected ADC reader. The `fn(u8) -> u16` seam cannot capture an `Adc` driver, so a
/// real reading needs a `'static` ADC handle; omitted here (analog pins report `0`) to keep the
/// example focused on the server/Wi-Fi bootstrap. See `GpioViewerBuilder::analog_source`.
fn read_adc(_gpio: u8) -> u16 {
    0
}

/// Runs the embassy-net stack (RX/TX + DHCP). Never returns. In esp-radio 0.17 the STA netif is a
/// `WifiDevice`, so the embassy-net `Runner` is generic over that (the 0.18 `Interface` alias is gone).
#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) -> ! {
    runner.run().await
}

/// Manages the Wi-Fi station: (re)configures, starts the radio, connects, reconnects on drop.
/// Never returns.
#[embassy_executor::task]
async fn wifi_connection_task(mut controller: WifiController<'static>) -> ! {
    // esp-radio 0.17 client config: SSID/password are owned `String`s (alloc), wrapped in
    // `ModeConfig::Client`. `set_config` only stores the config; `start_async` brings the radio up.
    let client_config = ClientConfig::default()
        .with_ssid(String::from(WIFI_SSID))
        .with_password(String::from(WIFI_PASSWORD));
    let config = ModeConfig::Client(client_config);

    // Configure + start the radio once. If either fails, retry the whole bring-up after a pause.
    loop {
        if let Err(error) = controller.set_config(&config) {
            println!("wifi: set_config failed: {error:?}; retrying in 5s");
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }
        if let Err(error) = controller.start_async().await {
            println!("wifi: start failed: {error:?}; retrying in 5s");
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }
        break;
    }

    // Connect / reconnect loop: on a successful connect, block until the link drops
    // (`WifiEvent::StaDisconnected`), then loop to reconnect.
    loop {
        match controller.connect_async().await {
            Ok(_) => {
                println!("wifi: connected to {WIFI_SSID}");
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                println!("wifi: disconnected; reconnecting");
            }
            Err(error) => {
                println!("wifi: connect failed: {error:?}; retrying in 5s");
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Drives the sampler loop, broadcasting frames to `/events`.
#[embassy_executor::task]
async fn sampler_task(viewer: &'static GpioViewer) {
    run_sampler(&EVENTS, viewer).await;
}

/// One HTTP/SSE worker: owns a TCP socket + buffers and serves connections on `DEFAULT_PORT`
/// forever. Several run concurrently (see [`WEB_POOL_SIZE`]) so a long-lived `/events` stream on
/// one socket never blocks the UI's REST fetches on another.
#[embassy_executor::task(pool_size = WEB_POOL_SIZE)]
async fn web_task(stack: Stack<'static>, state: &'static ServerState) -> ! {
    let mut rx_buffer = [0u8; 1536];
    let mut tx_buffer = [0u8; 2048];
    accept_loop(stack, DEFAULT_PORT, state, &mut rx_buffer, &mut tx_buffer).await
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // --- Chip + heap + embassy scheduler -------------------------------------------------
    let hal_config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(hal_config);

    // Heap split across the reclaimed RAM region and main DRAM. Both are required: esp-radio's
    // Wi-Fi DMA buffers must come from genuine internal DMA-capable SRAM (`dram_seg`), not the
    // reclaimed region (a reclaimed-only heap faults the first TCP connection with an unhandled
    // `WIFI_MAC` IRQ). Hardware on the esp32 proved 32 KB of `dram_seg` heap is enough and 16 KB is
    // not, so the main-DRAM region is fixed at 32 KB here too. With the hand-rolled server (no
    // ~97 KB picoserve future in `.bss`), main DRAM is plentiful and the S3 runs clean single-core.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 32 * 1024);

    // esp-rtos 0.2 `start` takes a single timer on Xtensa (the RISC-V-only software-interrupt arg
    // is `#[cfg(riscv)]`), so no `SoftwareInterruptControl` is needed here.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);
    println!("esp32s3: embassy + esp-rtos started");

    // --- Wi-Fi controller + station interface -------------------------------------------
    // esp-radio 0.17: `init()` (argless) returns a `Controller` that the Wi-Fi driver borrows for
    // its whole life, so it is parked in a `StaticCell` to give it a `'static` home. It requires the
    // esp-rtos scheduler to be running (started just above).
    static RADIO: StaticCell<esp_radio::Controller<'static>> = StaticCell::new();
    let radio = RADIO.init(esp_radio::init().expect("failed to initialize esp-radio"));
    let (controller, interfaces) =
        esp_radio::wifi::new(radio, peripherals.WIFI, WifiConfig::default())
            .expect("failed to initialize Wi-Fi controller");
    let station_device = interfaces.sta;

    // --- embassy-net stack with DHCPv4 --------------------------------------------------
    let net_config = embassy_net::Config::dhcpv4(Default::default());
    // Seed smoltcp's randomization from the factory MAC. esp-hal 1.0 returns a plain `[u8; 6]`.
    let mac_bytes = esp_hal::efuse::Efuse::read_base_mac_address();
    let mut seed: u64 = 0;
    for &byte in mac_bytes.iter() {
        seed = (seed << 8) | byte as u64;
    }

    static RESOURCES: StaticCell<StackResources<STACK_SOCKET_COUNT>> = StaticCell::new();
    let resources = RESOURCES.init(StackResources::new());
    let (stack, runner) = embassy_net::new(station_device, net_config, resources, seed);

    // embassy-executor 0.9 `spawn` returns `Result`; `must_spawn` panics on a full pool.
    spawner.must_spawn(net_task(runner));
    spawner.must_spawn(wifi_connection_task(controller));

    println!("esp32s3: waiting for Wi-Fi + DHCP...");
    stack.wait_config_up().await;
    let ipv4 = stack
        .config_v4()
        .expect("DHCP configuration missing after wait_config_up")
        .address
        .address();
    let mut ip_string: heapless::String<15> = heapless::String::new();
    let _ = write!(ip_string, "{ipv4}");
    println!("esp32s3: online at http://{ip_string}:{DEFAULT_PORT}/");

    // --- GPIOViewer configuration -------------------------------------------------------
    // Representative S3 pin mix: digital output (GPIO2), digital input (BOOT/GPIO0), a PWM/LEDC
    // pin (GPIO4), and an ADC pin (GPIO1 == ADC1_CH0). Adjust to match your wiring.
    let viewer = GpioViewer::builder()
        .port(DEFAULT_PORT)
        .register(2, PinMode::Output, PinType::Digital) // GPIO2 digital output
        .register(0, PinMode::Input, PinType::Digital) // GPIO0 BOOT button
        .pwm(4, 0, 13) // GPIO4 via LEDC channel 0, 13-bit resolution
        .adc(1) // GPIO1 (ADC1_CH0)
        .free_heap_source(|| esp_alloc::HEAP.free() as u32)
        .analog_source(read_adc)
        .build();

    static VIEWER: StaticCell<GpioViewer> = StaticCell::new();
    let viewer_ref: &'static GpioViewer = VIEWER.init(viewer);

    spawner.must_spawn(sampler_task(viewer_ref));

    // --- Partition table for `/partition` -----------------------------------------------
    // Serves the representative `DEFAULT_PARTITIONS`. To serve the board's REAL table, read it
    // once at boot with `esp_gpio_viewer::hwinfo::read_partition_infos` into a `'static` buffer
    // and inject the slice.
    let partitions = DEFAULT_PARTITIONS;

    // --- Serve --------------------------------------------------------------------------
    // Hand-rolled HTTP/1.1 + SSE server (no picoserve). `ServerState` is shared by reference — no
    // per-request clone. Several worker tasks accept concurrently so the persistent `/events`
    // stream never blocks the UI's REST fetches.
    static SERVER_STATE: StaticCell<ServerState> = StaticCell::new();
    let state: &'static ServerState = SERVER_STATE.init(ServerState::new(
        viewer_ref,
        ip_string.as_str(),
        "n/a",
        &EVENTS,
        partitions,
    ));

    for _ in 0..WEB_POOL_SIZE {
        spawner.must_spawn(web_task(stack, state));
    }

    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}
