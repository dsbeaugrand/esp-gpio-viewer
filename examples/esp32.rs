//! Runnable **ESP32** firmware for `esp-gpio-viewer`.
//!
//! Brings the board up end to end: Wi-Fi **STA** + DHCPv4 (esp-radio 0.18 + embassy-net),
//! the GPIOViewer HTTP/SSE API served by the crate's hand-rolled server, and the sampler
//! broadcasting live pin state. Point the hosted GPIOViewer UI (or a browser) at `http://<ip>:8080/`.
//!
//! ## Build & flash
//! ```bash
//! # Wi-Fi credentials are read at COMPILE time (never hard-coded / committed):
//! export WIFI_SSID="your-ssid"
//! export WIFI_PASSWORD="your-password"
//!
//! # Cross-compile (needs the `esp` toolchain):
//! cargo +esp build --release -Zbuild-std=core,alloc --target xtensa-esp32-none-elf \
//!     --example esp32 --features esp32,server
//!
//! # Flash + monitor (espflash):
//! cargo +esp run --release -Zbuild-std=core,alloc --target xtensa-esp32-none-elf \
//!     --example esp32 --features esp32,server
//! ```
//! See the project README for the `_secrets`-style note on setting the two env vars.

#![no_std]
#![no_main]

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::{sta::StationConfig, Config as WifiMode, Interface, WifiController};
use static_cell::StaticCell;

use esp_gpio_viewer::sampler::{run_sampler, FrameChannel};
use esp_gpio_viewer::server::{accept_loop, ServerState, DEFAULT_PARTITIONS};
use esp_gpio_viewer::{GpioViewer, PinMode, PinType, DEFAULT_PORT};

extern crate alloc;

// Required app-descriptor for the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

/// Wi-Fi credentials, injected at **compile time** via environment variables so no secret is
/// ever hard-coded or committed. The build fails clearly if either is unset.
const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

/// Number of concurrent HTTP/SSE worker tasks. The hand-rolled server has a tiny per-socket cost
/// (a couple of KB of buffers, a shallow poll — no 97 KB picoserve future), so we run several: one
/// can hold the long-lived `/events` SSE stream while the others handle the UI's parallel REST
/// fetches. Three gives comfortable headroom for the hosted UI's connection pattern.
const WEB_POOL_SIZE: usize = 3;

/// Sockets the embassy-net stack must hold open at once: the DHCP client plus one TCP socket
/// per web worker.
const STACK_SOCKET_COUNT: usize = WEB_POOL_SIZE + 1;

/// The sampler's broadcast channel: a `const`-constructible `static`, so `/events` subscribers
/// created per client outlive the request.
static EVENTS: FrameChannel = FrameChannel::new();

/// Firmware-injected ADC reader for `PinType::Analog` pins.
///
/// The injection seam is a plain `fn(u8) -> u16`, which cannot capture an `Adc` driver
/// instance. A real reading therefore requires a `'static` ADC handle (e.g. an
/// `Adc` behind a `Mutex` in a `StaticCell`) that this function reads; that peripheral wiring
/// is intentionally omitted here to keep the example focused on the server/Wi-Fi bootstrap, so
/// analog pins report `0`. See the crate docs on `GpioViewerBuilder::analog_source`.
fn read_adc(_gpio: u8) -> u16 {
    0
}

/// Runs the embassy-net stack (processes RX/TX + DHCP). Never returns.
#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) -> ! {
    runner.run().await
}

/// Manages the Wi-Fi station: (re)configures, connects, and reconnects on drop. Never returns.
#[embassy_executor::task]
async fn wifi_connection_task(mut controller: WifiController<'static>) -> ! {
    // Station config from the compile-time credentials. `set_config` also starts the radio
    // (esp-radio's `esp_wifi_start` runs inside it).
    // `with_password` takes an owned `String` (alloc); the SSID setter takes `impl Into<Ssid>`.
    let station_config = StationConfig::default()
        .with_ssid(WIFI_SSID)
        .with_password(WIFI_PASSWORD.into());
    let config = WifiMode::Station(station_config);

    loop {
        if let Err(error) = controller.set_config(&config) {
            println!("wifi: set_config failed: {error:?}; retrying in 5s");
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }

        match controller.connect_async().await {
            Ok(_) => {
                println!("wifi: connected to {WIFI_SSID}");
                // Block until the link drops, then loop to reconnect.
                let _ = controller.wait_for_disconnect_async().await;
                println!("wifi: disconnected; reconnecting");
            }
            Err(error) => {
                println!("wifi: connect failed: {error:?}; retrying in 5s");
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Drives the sampler loop, broadcasting `gpio-state` / `free_heap` frames to `/events`.
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

    // Heap regions, BOTH required. esp-radio's Wi-Fi driver allocates its DMA-capable RX/TX/AMPDU
    // buffers from the heap, and those must live in genuine internal DMA-capable SRAM (`dram_seg`):
    // a reclaimed-only heap faults the first TCP connection as an unhandled `WIFI_MAC` IRQ. Hardware
    // proved a 32 KB `dram_seg` heap is enough (16 KB is not). The bulk of the heap lives in the
    // reclaimed `dram2_seg` region (otherwise-unused ROM/RTC DRAM).
    //
    // With the hand-rolled server (no ~97 KB picoserve serve future in `.bss`), `dram_seg` is no
    // longer tight: the single embassy executor's main-task stack has ample room for the shallow
    // serve path, so this stays clean single-core.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 96 * 1024);
    esp_alloc::heap_allocator!(size: 32 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);
    println!("esp32: embassy + esp-rtos started");

    // --- Wi-Fi controller + station interface -------------------------------------------
    let (controller, interfaces) = esp_radio::wifi::new(peripherals.WIFI, Default::default())
        .expect("failed to initialize Wi-Fi controller");
    let station_device = interfaces.station;

    // --- embassy-net stack with DHCPv4 --------------------------------------------------
    let net_config = embassy_net::Config::dhcpv4(Default::default());
    // Seed smoltcp's port/sequence randomization from the factory MAC (unique per device).
    let mac_bytes = esp_hal::efuse::base_mac_address();
    let mut seed: u64 = 0;
    for &byte in mac_bytes.as_bytes() {
        seed = (seed << 8) | byte as u64;
    }

    static RESOURCES: StaticCell<StackResources<STACK_SOCKET_COUNT>> = StaticCell::new();
    let resources = RESOURCES.init(StackResources::new());
    let (stack, runner) = embassy_net::new(station_device, net_config, resources, seed);

    spawner.spawn(net_task(runner).expect("spawn net_task"));
    spawner.spawn(wifi_connection_task(controller).expect("spawn wifi_connection_task"));

    // Wait for the link + a DHCP lease, then learn our address.
    println!("esp32: waiting for Wi-Fi + DHCP...");
    stack.wait_config_up().await;
    let ipv4 = stack
        .config_v4()
        .expect("DHCP configuration missing after wait_config_up")
        .address
        .address();
    let mut ip_string: heapless::String<15> = heapless::String::new();
    let _ = write!(ip_string, "{ipv4}");
    println!("esp32: online at http://{ip_string}:{DEFAULT_PORT}/");

    // --- GPIOViewer configuration -------------------------------------------------------
    // A representative pin mix: a digital output (onboard LED), a digital input (BOOT button),
    // a PWM/LEDC pin, and an ADC pin. Adjust to match your wiring.
    let viewer = GpioViewer::builder()
        .port(DEFAULT_PORT)
        .register(2, PinMode::Output, PinType::Digital) // GPIO2 onboard LED
        .register(0, PinMode::Input, PinType::Digital) // GPIO0 BOOT button
        .pwm(4, 0, 13) // GPIO4 via LEDC channel 0, 13-bit resolution
        .adc(34) // GPIO34 (ADC1_CH6, input-only)
        .free_heap_source(|| esp_alloc::HEAP.free() as u32)
        .analog_source(read_adc)
        .build();
    static VIEWER: StaticCell<GpioViewer> = StaticCell::new();
    let viewer_ref: &'static GpioViewer = VIEWER.init(viewer);

    spawner.spawn(sampler_task(viewer_ref).expect("spawn sampler_task"));

    // The example serves the representative `DEFAULT_PARTITIONS`; see the crate docs for how to
    // inject the board's REAL flash table via `esp-bootloader-esp-idf`.
    let partitions = DEFAULT_PARTITIONS;

    // --- Serve --------------------------------------------------------------------------
    // Hand-rolled HTTP/1.1 + SSE server (no picoserve). `ServerState` is shared by reference — no
    // per-request clone. Several worker tasks accept concurrently so the persistent `/events`
    // stream never blocks the UI's REST fetches.
    static SERVER_STATE: StaticCell<ServerState> = StaticCell::new();
    let state: &'static ServerState = SERVER_STATE.init(ServerState::new(
        viewer_ref,
        ip_string.as_str(),
        // Free-sketch-RAM string shown on the index page.
        "n/a",
        &EVENTS,
        partitions,
    ));

    for _ in 0..WEB_POOL_SIZE {
        spawner.spawn(web_task(stack, state).expect("spawn web_task (pool full)"));
    }

    // main must never return; the spawned tasks own all the work.
    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}
