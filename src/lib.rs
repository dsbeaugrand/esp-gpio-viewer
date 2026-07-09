//! `esp-gpio-viewer` — a `no_std` ESP32 Rust port of the GPIOViewer backend.
//!
//! The upstream project (`gpio_viewer`, a single C++ header) runs an HTTP + SSE server
//! that feeds a remotely hosted Vue UI. Because that UI is hosted remotely, the only
//! surface we must port is the backend HTTP/SSE protocol — see [`protocol`] for the
//! byte-for-byte serializers.
//!
//! This crate is `no_std` for embedded builds and links `std` only under `cfg(test)`
//! so the host test suite (which validates the serializers) can run with plain `cargo test`.
//!
//! The crate provides the pure protocol serializers, the [`GpioViewer`]/[`GpioViewerBuilder`]
//! configuration types, a hand-rolled async HTTP/1.1 + SSE server over embassy-net (feature
//! `server`), the embassy sampler, and per-chip example firmware (`examples/`) that wires Wi-Fi
//! STA + DHCP, serves the routes, and runs the sampler. Firmware peripherals are supplied through allocator-/driver-
//! agnostic injection seams (`free_heap_source`, `free_psram_source`, `analog_source`).

#![cfg_attr(not(test), no_std)]

use heapless::{String, Vec};

pub mod capabilities;
// Pure HTTP/1.1 request-line + response-header helpers for the hand-rolled server. Compiled
// unconditionally (heapless only) so the host test suite validates them with plain `cargo test`;
// the embassy-net I/O that uses them lives in `server` behind the `server` feature.
pub mod http;
pub mod protocol;
pub mod value;

// `sampler` is compiled unconditionally so its pure change-diff (`diff_changes`) and that
// function's host unit tests run under plain `cargo test` (no features) — the one piece of
// the sampling path testable without hardware. The embassy sampler task + broadcast channel
// inside it are gated behind `server`, so host default builds still pull zero embassy deps.
pub mod sampler;

// The hand-rolled HTTP/SSE server lives behind the `server` feature: it depends on the embassy-*
// stack, which only builds for the Xtensa embedded targets. Host builds and `cargo test` (no
// features) never compile it, so the pure serializers + [`http`] helpers stay host-testable.
#[cfg(feature = "server")]
pub mod server;

// Chip/flash/heap readings for `/espinfo`. Gated behind `server` (its only consumer is the
// server-side handler); the real esp-hal reads inside are further gated by the chip features,
// so under `--features server` with no chip it compiles to the honest host fallback.
#[cfg(feature = "server")]
pub mod hwinfo;

// Register-level GPIO reads need `esp-hal`, so `reader` is gated behind `server` (its only
// consumer is the server-side sampler). The actual PAC access inside it is further gated by
// the chip features; under `--features server` with no chip it compiles to a host stub.
#[cfg(feature = "server")]
pub mod reader;

pub use value::{map_value, PinReading};

/// Maximum number of pins the builder registry can hold. Sized for the full ESP32 /
/// ESP32-S3 usable GPIO range with headroom.
pub const MAX_REGISTERED_PINS: usize = 48;

/// Default sampling interval in milliseconds (`gpio_viewer.h:256`).
pub const DEFAULT_SAMPLING_INTERVAL_MS: u32 = 100;

/// Default HTTP server port for the viewer.
pub const DEFAULT_PORT: u16 = 8080;

/// Default protocol release version (`gpio_viewer.h:24`).
pub const DEFAULT_RELEASE: &str = "1.7.1";

/// Pin type discriminant sent as `t` in a `gpio-state` frame.
///
/// Values mirror the C++ `enum pinTypes` (`gpio_viewer.h:100`): `digitalPin = 0`,
/// `PWMPin = 1`, `analogPin = 2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PinType {
    /// A plain digital I/O pin.
    Digital = 0,
    /// A PWM (LEDC) output pin.
    Pwm = 1,
    /// An analog (ADC) input pin.
    Analog = 2,
}

impl PinType {
    /// The raw integer discriminant emitted on the wire (`t`).
    pub fn to_int(self) -> u8 {
        self as u8
    }
}

/// Arduino pin modes used by the `/pinmodes` endpoint.
///
/// The C++ library stores raw Arduino mode integers; [`PinMode::to_int`] reproduces
/// those values so `/pinmodes` can emit them unchanged. Integer values follow the
/// Arduino-ESP32 core (`esp32-hal-gpio.h`): `INPUT = 0x01`, `OUTPUT = 0x03`,
/// `INPUT_PULLUP = 0x05`, `INPUT_PULLDOWN = 0x09`, `OUTPUT_OPEN_DRAIN = 0x13`,
/// `ANALOG = 0xC0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinMode {
    /// `INPUT`.
    Input,
    /// `OUTPUT`.
    Output,
    /// `INPUT_PULLUP`.
    InputPullup,
    /// `INPUT_PULLDOWN`.
    InputPulldown,
    /// `OUTPUT_OPEN_DRAIN`.
    OutputOpenDrain,
    /// `ANALOG`.
    Analog,
}

impl PinMode {
    /// The raw Arduino mode integer, as stored and emitted by the C++ library.
    pub fn to_int(self) -> u32 {
        match self {
            PinMode::Input => 0x01,
            PinMode::Output => 0x03,
            PinMode::InputPullup => 0x05,
            PinMode::InputPulldown => 0x09,
            PinMode::OutputOpenDrain => 0x13,
            PinMode::Analog => 0xC0,
        }
    }
}

/// Firmware-injected analog reader for ADC pins.
///
/// The ESP32 ADC is **not** register-readable the way GPIO input levels and LEDC duty are:
/// a one-shot conversion needs a live `esp_hal::analog::adc::Adc` driver plus a configured
/// `AdcPin` (attenuation + calibration state), all of which the *firmware* owns. To keep
/// this crate peripheral-agnostic — exactly as [`GpioViewer::free_heap_source`] keeps it
/// allocator-agnostic — the firmware supplies the reading through this plain `fn` pointer:
/// it takes the GPIO number and returns the raw 12-bit sample (`0..=4095`).
///
/// A plain `fn` pointer (rather than a `&'static mut dyn AnalogSource` trait object) is the
/// deliberate choice: it is `Copy`, so [`GpioViewer`] and the server's `AppState` stay
/// `Clone` (required by picoserve's `State` extractor), and it drops straight into embassy's
/// `'static` task bounds with no aliasing dance. The firmware stashes its `Adc` driver in a
/// `static` (just as `free_heap_source` reads the `esp_alloc::HEAP` static) and the `fn`
/// reads that static — see [`GpioViewerBuilder::analog_source`].
pub type AnalogSource = fn(u8) -> u16;

/// A pin registered with the viewer. The C++ version captured these implicitly through
/// preprocessor macros; the Rust port makes registration explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisteredPin {
    /// GPIO number.
    pub pin: u8,
    /// Arduino mode reported to `/pinmodes`.
    pub mode: PinMode,
    /// How this pin's value is sampled and reported.
    pub pin_type: PinType,
    /// LEDC channel backing a [`PinType::Pwm`] pin, used to locate the duty register
    /// (`getLedcChannelForPin`, `gpio_viewer.h:757`). `None` for non-PWM pins (and for PWM
    /// pins registered without a channel, which then read as duty `0`).
    pub pwm_channel: Option<u8>,
    /// LEDC timer resolution in bits for a [`PinType::Pwm`] pin. `max_duty = (1 << bits) - 1`
    /// is the denominator of the 8-bit scaling (`mapLedcReadTo8Bit`, `gpio_viewer.h:1062`).
    /// `None` for non-PWM pins.
    pub pwm_resolution_bits: Option<u8>,
}

/// Configured viewer state.
///
/// Carries the settings the protocol serializers and sampler need (sampling interval, port,
/// release string, registered pins) plus the firmware-injected source seams (free heap, free
/// PSRAM, analog reader). The per-chip examples (`examples/`) spawn the embassy tasks that
/// serve the router and run the sampler over this configuration.
#[derive(Debug, Clone)]
pub struct GpioViewer {
    /// Sampling interval in milliseconds.
    pub sampling_interval_ms: u32,
    /// HTTP server port.
    pub port: u16,
    /// Protocol release version served at `/release`.
    pub release: String<32>,
    /// The registered pin table backing `/pinmodes` and the sampler.
    pub pins: Vec<RegisteredPin, MAX_REGISTERED_PINS>,
    /// Firmware-injected free-heap source for the `free_heap` SSE frame
    /// (`esp_get_free_heap_size`, `gpio_viewer.h:711`).
    ///
    /// This crate stays allocator-agnostic: it does **not** depend on `esp-alloc`, so the
    /// firmware supplies the reading through this plain `fn` pointer (`no_std`-friendly,
    /// `Copy`, and `'static`-storable). When `None`, the sampler falls back to `0` — the
    /// `free_heap` frame shape stays correct but the value is static (heartbeat-only).
    ///
    /// Firmware wiring:
    /// ```ignore
    /// let viewer = GpioViewer::builder()
    ///     .free_heap_source(|| esp_alloc::HEAP.free() as u32)
    ///     .build();
    /// ```
    pub free_heap_source: Option<fn() -> u32>,
    /// Firmware-injected free-PSRAM source for `/free_psram` and the `free_psram` SSE frame
    /// (`checkFreePSRAM`, `gpio_viewer.h:720`).
    ///
    /// Mirrors [`Self::free_heap_source`] to keep the crate allocator-agnostic: the firmware
    /// owns the PSRAM allocator and supplies the reading through this plain `fn` pointer. When
    /// `None`, the board is reported as having **no PSRAM** (the `... No PSRAM` response shape),
    /// exactly matching the C++ behaviour on chips without external SPIRAM. A board that *does*
    /// have PSRAM injects a source, e.g.:
    /// ```ignore
    /// let viewer = GpioViewer::builder()
    ///     .free_psram_source(|| esp_alloc::psram_free() as u32)
    ///     .build();
    /// ```
    pub free_psram_source: Option<fn() -> u32>,
    /// Firmware-injected ADC reader for [`PinType::Analog`] pins (`analogRead`,
    /// `gpio_viewer.h:1040`). See [`AnalogSource`] for why this is a `fn` pointer rather than
    /// a trait object. When `None`, analog pins read as raw `0` — the frame shape stays
    /// correct but the value is static until firmware injects a source.
    pub analog_source: Option<AnalogSource>,
}

impl GpioViewer {
    /// Start building a viewer with default settings.
    pub fn builder() -> GpioViewerBuilder {
        GpioViewerBuilder::new()
    }

    /// The registered pins as `(pin, mode_int)` tuples, ready for
    /// [`protocol::pinmodes_body`].
    pub fn pinmode_pairs(&self) -> Vec<(u8, u32), MAX_REGISTERED_PINS> {
        let mut pairs = Vec::new();
        for registered in &self.pins {
            // Capacity matches `self.pins`, so this push cannot fail.
            let _ = pairs.push((registered.pin, registered.mode.to_int()));
        }
        pairs
    }
}

/// Builder for [`GpioViewer`]. Minimal for this task — enough to back the serializers.
#[derive(Debug, Clone)]
pub struct GpioViewerBuilder {
    sampling_interval_ms: u32,
    port: u16,
    release: String<32>,
    pins: Vec<RegisteredPin, MAX_REGISTERED_PINS>,
    free_heap_source: Option<fn() -> u32>,
    free_psram_source: Option<fn() -> u32>,
    analog_source: Option<AnalogSource>,
}

impl GpioViewerBuilder {
    /// Create a builder pre-populated with the library defaults.
    pub fn new() -> Self {
        let mut release: String<32> = String::new();
        // DEFAULT_RELEASE is short and well within the 32-byte buffer.
        let _ = release.push_str(DEFAULT_RELEASE);
        GpioViewerBuilder {
            sampling_interval_ms: DEFAULT_SAMPLING_INTERVAL_MS,
            port: DEFAULT_PORT,
            release,
            pins: Vec::new(),
            // No heap source by default; the sampler falls back to 0 until firmware injects one.
            free_heap_source: None,
            // No PSRAM source by default; `/free_psram` reports "No PSRAM" until firmware injects one.
            free_psram_source: None,
            // No ADC source by default; analog pins read 0 until firmware injects one.
            analog_source: None,
        }
    }

    /// Override the sampling interval (milliseconds).
    pub fn sampling_interval_ms(mut self, sampling_interval_ms: u32) -> Self {
        self.sampling_interval_ms = sampling_interval_ms;
        self
    }

    /// Override the HTTP server port.
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Override the release version string. Truncated to the 32-byte buffer if longer.
    pub fn release(mut self, release: &str) -> Self {
        self.release.clear();
        // push_str truncates on overflow; version strings are far shorter than 32 bytes.
        let _ = self.release.push_str(release);
        self
    }

    /// Register a pin. Silently ignores the request if the registry is full
    /// ([`MAX_REGISTERED_PINS`]).
    ///
    /// For PWM and ADC pins prefer the ergonomic [`Self::pwm`] / [`Self::adc`] helpers, which
    /// fill in the channel/resolution/mode; this low-level form leaves the PWM channel and
    /// resolution unset (a PWM pin so registered reads as duty `0`).
    pub fn register(mut self, pin: u8, mode: PinMode, pin_type: PinType) -> Self {
        let _ = self.pins.push(RegisteredPin {
            pin,
            mode,
            pin_type,
            pwm_channel: None,
            pwm_resolution_bits: None,
        });
        self
    }

    /// Register a PWM (LEDC) output pin bound to `channel` at `resolution_bits`.
    ///
    /// The reader reads that LEDC channel's live duty register non-invasively and scales it
    /// against `max_duty = (1 << resolution_bits) - 1` (`mapLedcReadTo8Bit`,
    /// `gpio_viewer.h:1062`). The pin reports `PinMode::Output` to `/pinmodes`, matching how
    /// Arduino configures a `ledcAttach`-ed pin (an OUTPUT). Silently ignored when the
    /// registry is full ([`MAX_REGISTERED_PINS`]).
    pub fn pwm(mut self, pin: u8, channel: u8, resolution_bits: u8) -> Self {
        let _ = self.pins.push(RegisteredPin {
            pin,
            mode: PinMode::Output,
            pin_type: PinType::Pwm,
            pwm_channel: Some(channel),
            pwm_resolution_bits: Some(resolution_bits),
        });
        self
    }

    /// Register an analog (ADC) input pin.
    ///
    /// The raw 12-bit reading is supplied at runtime by the firmware-injected
    /// [`Self::analog_source`]; the pin reports `PinMode::Analog` (`0xC0`) to `/pinmodes`.
    /// Silently ignored when the registry is full ([`MAX_REGISTERED_PINS`]).
    pub fn adc(mut self, pin: u8) -> Self {
        let _ = self.pins.push(RegisteredPin {
            pin,
            mode: PinMode::Analog,
            pin_type: PinType::Analog,
            pwm_channel: None,
            pwm_resolution_bits: None,
        });
        self
    }

    /// Inject the firmware's ADC reader for [`PinType::Analog`] pins.
    ///
    /// Keeps the library peripheral-agnostic (no ownership of the `Adc` driver): the firmware
    /// owns the ADC and passes a `fn` that reads it, e.g.
    /// `.analog_source(|gpio| read_adc_from_static(gpio))`. See [`AnalogSource`]. Without
    /// this, analog pins report raw `0`.
    pub fn analog_source(mut self, source: AnalogSource) -> Self {
        self.analog_source = Some(source);
        self
    }

    /// Inject the firmware's free-heap reader for the `free_heap` SSE frame.
    ///
    /// Keeps the library allocator-agnostic (no `esp-alloc` dependency): the firmware owns
    /// the allocator and passes a `fn` pointer that reads it, e.g.
    /// `.free_heap_source(|| esp_alloc::HEAP.free() as u32)`. See
    /// [`GpioViewer::free_heap_source`]. Without this, the sampler reports `0`.
    pub fn free_heap_source(mut self, source: fn() -> u32) -> Self {
        self.free_heap_source = Some(source);
        self
    }

    /// Inject the firmware's free-PSRAM reader for `/free_psram` and the `free_psram` SSE frame.
    ///
    /// Keeps the library allocator-agnostic (no `esp-alloc` dependency): the firmware owns the
    /// PSRAM allocator and passes a `fn` pointer that reads it, e.g.
    /// `.free_psram_source(|| esp_alloc::psram_free() as u32)`. See
    /// [`GpioViewer::free_psram_source`]. Without this, `/free_psram` reports "No PSRAM"
    /// (`checkFreePSRAM`, `gpio_viewer.h:720`).
    pub fn free_psram_source(mut self, source: fn() -> u32) -> Self {
        self.free_psram_source = Some(source);
        self
    }

    /// Finalize the configuration into a [`GpioViewer`].
    pub fn build(self) -> GpioViewer {
        GpioViewer {
            sampling_interval_ms: self.sampling_interval_ms,
            port: self.port,
            release: self.release,
            pins: self.pins,
            free_heap_source: self.free_heap_source,
            free_psram_source: self.free_psram_source,
            analog_source: self.analog_source,
        }
    }
}

impl Default for GpioViewerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_type_int_values() {
        assert_eq!(PinType::Digital.to_int(), 0);
        assert_eq!(PinType::Pwm.to_int(), 1);
        assert_eq!(PinType::Analog.to_int(), 2);
    }

    #[test]
    fn pin_mode_int_values() {
        assert_eq!(PinMode::Input.to_int(), 0x01);
        assert_eq!(PinMode::Output.to_int(), 0x03);
        assert_eq!(PinMode::InputPullup.to_int(), 0x05);
        assert_eq!(PinMode::InputPulldown.to_int(), 0x09);
        assert_eq!(PinMode::OutputOpenDrain.to_int(), 0x13);
        assert_eq!(PinMode::Analog.to_int(), 0xC0);
    }

    #[test]
    fn builder_defaults() {
        let viewer = GpioViewer::builder().build();
        assert_eq!(viewer.sampling_interval_ms, DEFAULT_SAMPLING_INTERVAL_MS);
        assert_eq!(viewer.port, DEFAULT_PORT);
        assert_eq!(viewer.release.as_str(), DEFAULT_RELEASE);
        assert!(viewer.pins.is_empty());
    }

    #[test]
    fn builder_registers_pins_and_emits_pairs() {
        let viewer = GpioViewer::builder()
            .sampling_interval_ms(250)
            .port(9000)
            .release("2.0.0")
            .register(2, PinMode::Output, PinType::Digital)
            .register(15, PinMode::InputPullup, PinType::Digital)
            .build();

        assert_eq!(viewer.sampling_interval_ms, 250);
        assert_eq!(viewer.port, 9000);
        assert_eq!(viewer.release.as_str(), "2.0.0");

        let pairs = viewer.pinmode_pairs();
        assert_eq!(pairs.as_slice(), &[(2u8, 0x03u32), (15u8, 0x05u32)]);
        // Round-trip through the serializer to confirm the pieces fit together.
        assert_eq!(
            protocol::pinmodes_body(pairs.as_slice()).as_str(),
            "[{\"pin\":\"2\",\"mode\":\"3\"},{\"pin\":\"15\",\"mode\":\"5\"}]"
        );
    }

    #[test]
    fn builder_pwm_registers_channel_and_resolution() {
        // .pwm() sets PinType::Pwm, mode Output (0x03), and stores channel + resolution.
        let viewer = GpioViewer::builder().pwm(18, 3, 10).build();

        let registered = viewer.pins.first().expect("one PWM pin registered");
        assert_eq!(registered.pin, 18);
        assert_eq!(registered.pin_type, PinType::Pwm);
        assert_eq!(registered.mode, PinMode::Output);
        assert_eq!(registered.pwm_channel, Some(3));
        assert_eq!(registered.pwm_resolution_bits, Some(10));

        // A PWM pin still reports a sensible Arduino mode (OUTPUT = 0x03) to /pinmodes.
        let pairs = viewer.pinmode_pairs();
        assert_eq!(pairs.as_slice(), &[(18u8, 0x03u32)]);
    }

    #[test]
    fn builder_adc_registers_analog_pin() {
        // .adc() sets PinType::Analog and mode ANALOG (0xC0), leaving the PWM fields unset.
        let viewer = GpioViewer::builder().adc(34).build();

        let registered = viewer.pins.first().expect("one ADC pin registered");
        assert_eq!(registered.pin, 34);
        assert_eq!(registered.pin_type, PinType::Analog);
        assert_eq!(registered.mode, PinMode::Analog);
        assert_eq!(registered.pwm_channel, None);
        assert_eq!(registered.pwm_resolution_bits, None);

        // ANALOG mode (0xC0 = 192) is emitted to /pinmodes.
        let pairs = viewer.pinmode_pairs();
        assert_eq!(pairs.as_slice(), &[(34u8, 192u32)]);
    }

    #[test]
    fn builder_analog_source_is_stored() {
        // The injected ADC reader is retained on the built viewer (parallel to
        // free_heap_source); without one, analog_source stays None.
        fn fake_adc(_gpio: u8) -> u16 {
            2048
        }
        let injected = GpioViewer::builder().analog_source(fake_adc).build();
        assert!(injected.analog_source.is_some());
        assert_eq!(injected.analog_source.unwrap()(0), 2048);

        let absent = GpioViewer::builder().build();
        assert!(absent.analog_source.is_none());
    }

    #[test]
    fn builder_mixes_all_pin_types() {
        // Digital, PWM, and ADC registrations coexist in one registry in order.
        let viewer = GpioViewer::builder()
            .register(2, PinMode::Output, PinType::Digital)
            .pwm(18, 0, 8)
            .adc(36)
            .build();

        let types: Vec<PinType, MAX_REGISTERED_PINS> =
            viewer.pins.iter().map(|pin| pin.pin_type).collect();
        assert_eq!(
            types.as_slice(),
            &[PinType::Digital, PinType::Pwm, PinType::Analog]
        );
    }
}
