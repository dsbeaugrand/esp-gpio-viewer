//! Sampling: the pure change-diff (host-tested) plus the embassy sampler task (feature
//! `server`).
//!
//! The C++ reference runs one `monitorTask` (`gpio_viewer.h:736`) that, every
//! `samplingInterval`, calls `checkGPIOValues` (`:676`) to diff each pin against the last
//! sent value and emit **only the changed pins**, then `checkFreeHeap` (`:709`) to emit the
//! free-heap string on change, and finally a 1000 ms heartbeat that resends the last
//! free-heap value when nothing changed (`:746-759`).
//!
//! ## What is host-testable
//! The one piece that needs no hardware is the change diff. It lives in [`diff_changes`],
//! a pure function over previous raw values + current readings. It is compiled
//! unconditionally (not behind `server`) so the host test suite exercises it with plain
//! `cargo test` — no esp/embassy/picoserve dependencies pulled in.
//!
//! ## What needs the runtime
//! The embassy task, the broadcast channel, and the hardware reads live in the
//! `#[cfg(feature = "server")]` [`runtime`] module and are re-exported below.

use heapless::Vec;

use crate::value::PinReading;
use crate::RegisteredPin;

/// One changed pin in the exact tuple shape [`crate::protocol::gpio_state_body`] consumes:
/// `(pin, scaled `s`, raw `v`, pin-type `t`)`.
pub type PinChange = (u8, u32, u32, u8);

/// Diff current readings against the previously-sent raw values, reproducing
/// `checkGPIOValues` (`gpio_viewer.h:676-707`).
///
/// A pin is emitted **only when its raw value `v` changed** — exactly the C++ test
/// `originalValue != lastPinStates[i]` (`:688`). For each emitted pin the matching
/// `previous[i]` slot is updated to the new raw value (`:695`), so the next call diffs
/// against the value just sent.
///
/// `pins`, `readings`, and `previous` are parallel-indexed (entry `i` of each describes the
/// same registered pin). Iteration stops at the shortest of the three, which keeps the
/// function total and panic-free even if a caller passes mismatched lengths. Changed pins
/// are pushed into `out` (cleared first); if `out` fills to capacity `N`, further changes
/// are dropped rather than panicking, matching the graceful-overflow style of the crate.
pub fn diff_changes<const N: usize>(
    pins: &[RegisteredPin],
    readings: &[PinReading],
    previous: &mut [u32],
    out: &mut Vec<PinChange, N>,
) {
    out.clear();
    for (index, (registered, reading)) in pins.iter().zip(readings.iter()).enumerate() {
        // Never index past the caller's previous-state buffer.
        if index >= previous.len() {
            break;
        }
        if reading.raw != previous[index] {
            // Record the newly-sent value so it is not re-emitted next cycle (`:695`).
            previous[index] = reading.raw;
            // Capacity-bounded push: a full `out` silently drops extras (graceful overflow).
            let _ = out.push((
                registered.pin,
                reading.scaled,
                reading.raw,
                reading.pin_type.to_int(),
            ));
        }
    }
}

/// Free-heap byte count fallback when the firmware injects no source.
///
/// The C++ reads `esp_get_free_heap_size()` (`gpio_viewer.h:711`); this crate stays
/// allocator-agnostic (no `esp-alloc` dependency), so the firmware supplies the reading via
/// [`crate::GpioViewer::free_heap_source`]. When it does not, this fallback keeps the
/// `free_heap` frame *shape* correct with a static value (heartbeat-only).
pub const FREE_HEAP_FALLBACK: u32 = 0;

/// Resolve the free-heap byte count from an optional firmware-injected source.
///
/// Pure and host-testable: calls the injected `fn` when `Some`, else returns
/// [`FREE_HEAP_FALLBACK`]. This is the whole of the heap-source decision logic, extracted so
/// it can be unit-tested without a running sampler or any hardware.
pub fn resolve_free_heap(source: Option<fn() -> u32>) -> u32 {
    match source {
        Some(read_heap) => read_heap(),
        None => FREE_HEAP_FALLBACK,
    }
}

/// Resolve the free-PSRAM byte count from an optional firmware-injected source.
///
/// Pure and host-testable, mirroring [`resolve_free_heap`] but returning an [`Option`]: `Some`
/// only when the firmware injected a PSRAM source (the board has PSRAM), otherwise `None`
/// (reported as "No PSRAM"). This maps one-to-one onto the `Option<u32>` argument that
/// [`crate::protocol::free_psram_body`] consumes, so `/free_psram` renders the exact C++
/// `checkFreePSRAM` shape (`gpio_viewer.h:720`) for both the PSRAM and no-PSRAM cases.
pub fn resolve_free_psram(source: Option<fn() -> u32>) -> Option<u32> {
    source.map(|read_psram| read_psram())
}

/// Read every registered pin into a parallel-indexed readings buffer, mirroring the per-pin
/// `readGPIO` sweep the C++ `checkGPIOValues` performs (`gpio_viewer.h:684-686`).
///
/// Shared by the connect-time baseline (SSE handler) and the sampler loop so both observe
/// pins identically. Capacity-bounded to [`MAX_REGISTERED_PINS`]; pins beyond that are
/// ignored (the builder caps registration at the same limit).
/// `analog_source` is the firmware-injected ADC reader (from
/// [`crate::GpioViewer::analog_source`]), threaded through to the analog pins' [`read_pin`]
/// arm; digital and PWM pins ignore it and read hardware registers directly.
///
/// [`read_pin`]: crate::reader::read_pin
#[cfg(feature = "server")]
pub fn read_all(
    pins: &[RegisteredPin],
    analog_source: Option<crate::AnalogSource>,
    out: &mut Vec<PinReading, { crate::MAX_REGISTERED_PINS }>,
) {
    out.clear();
    for registered in pins.iter() {
        if out
            .push(crate::reader::read_pin(registered, analog_source))
            .is_err()
        {
            break;
        }
    }
}

// -----------------------------------------------------------------------------------------
// Server runtime: broadcast channel + embassy sampler task. Only compiled under `server`,
// which pulls in embassy-sync/-time; the pure diff above stays host-testable without them.
// -----------------------------------------------------------------------------------------
#[cfg(feature = "server")]
mod runtime {
    use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
    use embassy_sync::pubsub::PubSubChannel;
    use embassy_time::{Duration, Instant, Timer};
    use heapless::{String, Vec};

    use super::{diff_changes, read_all, resolve_free_heap, PinChange};
    use crate::value::PinReading;
    use crate::{protocol, GpioViewer, MAX_REGISTERED_PINS};

    /// Buffer capacity of a `gpio-state` SSE payload — matches
    /// [`crate::protocol::gpio_state_body`]'s `String<2048>`.
    pub const GPIO_STATE_CAP: usize = 2048;

    /// Buffer capacity of a `free_heap` / `free_psram` payload — matches
    /// [`crate::protocol::format_bytes`]'s `String<24>`.
    pub const BYTES_STR_CAP: usize = 24;

    /// Number of queued frames the broadcast channel buffers before evicting the oldest.
    /// Small: the sampler publishes at most a few frames per interval and SSE clients drain
    /// promptly; a slow client coalesces via `Lagged` rather than stalling the sampler.
    ///
    /// Kept low (2) on this `esp-hal-1.0` branch: each queued frame is a `String<2048>`, so the
    /// channel storage is `FRAME_QUEUE_DEPTH * ~2 KiB`. On a device whose internal RAM is
    /// already nearly full (LVGL pool + Wi-Fi driver heap), a depth of 8 (~16 KiB) does not fit;
    /// depth 2 (~4 KiB) leaves headroom while still coalescing bursts. (`master` keeps 8.)
    pub const FRAME_QUEUE_DEPTH: usize = 2;

    /// Maximum simultaneous SSE subscribers (each `/events` connection takes one slot).
    pub const MAX_SSE_CLIENTS: usize = 4;

    /// Heartbeat window: resend the last `free_heap` after this long with no changes
    /// (`sentIntervalIfNoActivity`, `gpio_viewer.h:746-753`).
    pub const HEARTBEAT_MS: u64 = 1000;

    /// A frame broadcast from the sampler to every connected SSE client.
    ///
    /// The enum variants map 1:1 onto the SSE event names the hosted UI expects
    /// (`gpio-state`, `free_heap`, `free_psram`); the SSE handler in
    /// [`crate::server`] performs that mapping. Cloneable because each subscriber receives
    /// its own copy from the [`PubSubChannel`].
    // The `gpio-state` variant is intentionally large (a stack-inline `String<2048>`): this
    // is a `no_std` crate with no guaranteed allocator, so the clippy-suggested `Box` is not
    // available. The size is bounded and the channel depth is small, so the fixed cost is
    // acceptable and deliberate.
    #[allow(clippy::large_enum_variant)]
    #[derive(Debug, Clone)]
    pub enum Frame {
        /// `event: gpio-state` — changed pins only (`checkGPIOValues`, `gpio_viewer.h:676`).
        GpioState(String<GPIO_STATE_CAP>),
        /// `event: free_heap` — free-heap string on change or heartbeat (`:709`, `:752`).
        FreeHeap(String<BYTES_STR_CAP>),
        /// `event: free_psram` — free-PSRAM string on change (`checkFreePSRAM`, `:721`).
        ///
        /// Plumbed end-to-end so the event name is available, but the sampler does **not**
        /// currently produce it: PSRAM stats need `esp-psram`, which (like `esp-alloc`) is
        /// out of scope for this task. A later task fills this in the same way as
        /// [`Frame::FreeHeap`].
        FreePsram(String<BYTES_STR_CAP>),
    }

    /// The broadcast channel type shared between the sampler (single publisher) and the SSE
    /// handlers (one subscriber per client). Declared `static` by the runtime/examples task;
    /// [`PubSubChannel::new`] is `const`, so it can back a `static`.
    pub type FrameChannel =
        PubSubChannel<CriticalSectionRawMutex, Frame, FRAME_QUEUE_DEPTH, MAX_SSE_CLIENTS, 1>;

    /// The sampler loop: the embassy-side port of `monitorTask` (`gpio_viewer.h:736-762`).
    ///
    /// Runs forever, so all borrows are `'static`. It is exposed as a plain `async fn`
    /// (not a spawned `#[embassy_executor::task]`) on purpose: this crate does not depend on
    /// `embassy-executor`, and the WiFi / esp-rtos bootstrap belongs to the per-chip
    /// examples task (`esp-gpio-viewer-akb`). That task wraps this in its own spawned task,
    /// e.g.:
    ///
    /// ```ignore
    /// static EVENTS: FrameChannel = FrameChannel::new();
    /// static VIEWER: StaticCell<GpioViewer> = StaticCell::new();
    ///
    /// #[embassy_executor::task]
    /// async fn sampler_task(viewer: &'static GpioViewer) {
    ///     esp_gpio_viewer::sampler::run_sampler(&EVENTS, viewer).await;
    /// }
    /// // ... then serve `build_router()` with an `AppState { events: &EVENTS, .. }`.
    /// ```
    ///
    /// Each cycle: diff all pins and broadcast a `gpio-state` frame on any change
    /// (`checkGPIOValues`); broadcast a `free_heap` frame on heap change (`checkFreeHeap`);
    /// and, if nothing changed for [`HEARTBEAT_MS`], resend the last free-heap value as a
    /// pulse (`monitorTask` heartbeat).
    pub async fn run_sampler(channel: &'static FrameChannel, viewer: &'static GpioViewer) {
        // Immediate (non-blocking) publisher: a full queue evicts the oldest frame rather
        // than back-pressuring the sampler, keeping the sampling cadence steady.
        let publisher = channel.immediate_publisher();
        let pins = viewer.pins.as_slice();
        let analog_source = viewer.analog_source;
        let interval = Duration::from_millis(viewer.sampling_interval_ms as u64);

        // Prime the previous-state table from an initial read, mirroring `resetStatePins`
        // (`gpio_viewer.h:663`): the first loop then reports only genuine changes.
        let mut previous = [0u32; MAX_REGISTERED_PINS];
        let mut readings: Vec<PinReading, MAX_REGISTERED_PINS> = Vec::new();
        read_all(pins, analog_source, &mut readings);
        for (index, reading) in readings.iter().enumerate() {
            if index >= previous.len() {
                break;
            }
            previous[index] = reading.raw;
        }

        // Resolve free heap through the firmware-injected source (or the 0 fallback).
        let free_heap_source = viewer.free_heap_source;
        let mut last_heap = resolve_free_heap(free_heap_source);
        let mut last_activity = Instant::now();
        let mut changes: Vec<PinChange, MAX_REGISTERED_PINS> = Vec::new();

        loop {
            // --- gpio-state: diff and broadcast only changed pins (checkGPIOValues) --------
            read_all(pins, analog_source, &mut readings);
            diff_changes(pins, &readings, &mut previous, &mut changes);
            let mut had_change = false;
            if !changes.is_empty() {
                publisher.publish_immediate(Frame::GpioState(protocol::gpio_state_body(&changes)));
                had_change = true;
            }

            // --- free_heap: broadcast on change (checkFreeHeap, gpio_viewer.h:709) ---------
            let heap = resolve_free_heap(free_heap_source);
            if heap != last_heap {
                last_heap = heap;
                publisher.publish_immediate(Frame::FreeHeap(protocol::format_bytes(heap)));
                had_change = true;
            }

            // --- heartbeat: resend last free_heap after HEARTBEAT_MS idle (:746-759) -------
            if had_change {
                last_activity = Instant::now();
            } else if last_activity.elapsed() >= Duration::from_millis(HEARTBEAT_MS) {
                publisher.publish_immediate(Frame::FreeHeap(protocol::format_bytes(last_heap)));
                last_activity = Instant::now();
            }

            Timer::after(interval).await;
        }
    }
}

#[cfg(feature = "server")]
pub use runtime::{
    run_sampler, Frame, FrameChannel, BYTES_STR_CAP, FRAME_QUEUE_DEPTH, GPIO_STATE_CAP,
    HEARTBEAT_MS, MAX_SSE_CLIENTS,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PinMode, PinType, RegisteredPin};

    /// Build a digital `RegisteredPin` for tests (mode is irrelevant to the diff).
    fn digital_pin(pin: u8) -> RegisteredPin {
        RegisteredPin {
            pin,
            mode: PinMode::Input,
            pin_type: PinType::Digital,
            pwm_channel: None,
            pwm_resolution_bits: None,
        }
    }

    #[test]
    fn resolve_free_heap_uses_injected_source() {
        // A firmware-injected source is called and its value returned verbatim.
        fn heap_reader() -> u32 {
            123_456
        }
        assert_eq!(resolve_free_heap(Some(heap_reader)), 123_456);
    }

    #[test]
    fn resolve_free_heap_falls_back_when_absent() {
        // No source -> documented 0 fallback (frame shape stays correct, value static).
        assert_eq!(resolve_free_heap(None), FREE_HEAP_FALLBACK);
        assert_eq!(resolve_free_heap(None), 0);
    }

    #[test]
    fn resolve_free_psram_returns_some_when_source_present() {
        // A board with PSRAM injects a source; the value is returned as `Some(bytes)`.
        fn psram_reader() -> u32 {
            2_097_152
        }
        assert_eq!(resolve_free_psram(Some(psram_reader)), Some(2_097_152));
    }

    #[test]
    fn resolve_free_psram_returns_none_when_absent() {
        // No source -> `None`, which `/free_psram` renders as the "No PSRAM" shape.
        assert_eq!(resolve_free_psram(None), None);
    }

    #[test]
    fn diff_no_change_emits_nothing() {
        // Previous already equals the current raw values -> empty change set.
        let pins = [digital_pin(2), digital_pin(4)];
        let readings = [PinReading::digital(true), PinReading::digital(false)];
        let mut previous = [1u32, 0u32]; // matches high(=1) / low(=0)
        let mut out: Vec<PinChange, 8> = Vec::new();

        diff_changes(&pins, &readings, &mut previous, &mut out);

        assert!(out.is_empty());
        // Previous state is untouched when nothing changed.
        assert_eq!(previous, [1u32, 0u32]);
    }

    #[test]
    fn diff_single_change_emits_one_and_updates_previous() {
        // Pin 2 goes low->high (raw 0 -> 1); pin 4 stays low.
        let pins = [digital_pin(2), digital_pin(4)];
        let readings = [PinReading::digital(true), PinReading::digital(false)];
        let mut previous = [0u32, 0u32];
        let mut out: Vec<PinChange, 8> = Vec::new();

        diff_changes(&pins, &readings, &mut previous, &mut out);

        // Only pin 2 changed: (pin=2, s=256, v=1, t=0).
        assert_eq!(out.as_slice(), &[(2u8, 256u32, 1u32, 0u8)]);
        // The changed slot is advanced to the new raw value; the unchanged one is left as-is.
        assert_eq!(previous, [1u32, 0u32]);
    }

    #[test]
    fn diff_multiple_changes_preserve_registration_order() {
        // Both pins change: pin 2 low->high, pin 4 high->low.
        let pins = [digital_pin(2), digital_pin(4)];
        let readings = [PinReading::digital(true), PinReading::digital(false)];
        let mut previous = [0u32, 1u32];
        let mut out: Vec<PinChange, 8> = Vec::new();

        diff_changes(&pins, &readings, &mut previous, &mut out);

        assert_eq!(
            out.as_slice(),
            &[(2u8, 256u32, 1u32, 0u8), (4u8, 0u32, 0u32, 0u8)]
        );
        assert_eq!(previous, [1u32, 0u32]);
    }

    #[test]
    fn diff_clears_stale_output_before_filling() {
        // A pre-populated `out` must be cleared so it only ever holds the current cycle.
        let pins = [digital_pin(7)];
        let readings = [PinReading::digital(true)];
        let mut previous = [0u32];
        let mut out: Vec<PinChange, 8> = Vec::new();
        let _ = out.push((99u8, 1u32, 1u32, 0u8)); // stale entry from a "previous cycle"

        diff_changes(&pins, &readings, &mut previous, &mut out);

        // Stale entry gone; only pin 7's change remains.
        assert_eq!(out.as_slice(), &[(7u8, 256u32, 1u32, 0u8)]);
    }

    #[test]
    fn diff_round_trips_through_gpio_state_body() {
        // The tuple shape must feed protocol::gpio_state_body byte-for-byte (task-1 serializer).
        let pins = [digital_pin(2), digital_pin(4)];
        let readings = [PinReading::digital(true), PinReading::digital(false)];
        let mut previous = [0u32, 1u32];
        let mut out: Vec<PinChange, 8> = Vec::new();

        diff_changes(&pins, &readings, &mut previous, &mut out);

        assert_eq!(
            crate::protocol::gpio_state_body(&out).as_str(),
            "{\"2\": {\"s\": 256, \"v\": 1, \"t\": 0}, \"4\": {\"s\": 0, \"v\": 0, \"t\": 0}}"
        );
    }

    #[test]
    fn diff_stops_at_shortest_slice_without_panicking() {
        // A `previous` shorter than `pins`/`readings` must not index out of bounds.
        let pins = [digital_pin(2), digital_pin(4), digital_pin(5)];
        let readings = [
            PinReading::digital(true),
            PinReading::digital(true),
            PinReading::digital(true),
        ];
        let mut previous = [0u32, 0u32]; // shorter than the 3 pins
        let mut out: Vec<PinChange, 8> = Vec::new();

        diff_changes(&pins, &readings, &mut previous, &mut out);

        // Only the two in-range pins are considered.
        assert_eq!(
            out.as_slice(),
            &[(2u8, 256u32, 1u32, 0u8), (4u8, 256u32, 1u32, 0u8)]
        );
    }
}
