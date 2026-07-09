//! Non-invasive, register-level GPIO reads (feature `server`).
//!
//! The C++ reference reads pin state through Arduino's `digitalRead` / `ledcRead` /
//! `analogRead` inside `readGPIO` (`gpio_viewer.h:1010`). Crucially, GPIOViewer must
//! observe pins the *application* owns without taking ownership itself, so it reads the
//! GPIO **input registers directly** rather than constructing esp-hal `Input` drivers
//! (which would require owning each pin).
//!
//! This module reproduces the DIGITAL arm of `readGPIO` by reading the GPIO input
//! registers straight from the esp-hal PAC:
//!  * pins `0..=31`  -> `GPIO.in`  (`GPIO::regs().in_()`)
//!  * pins `>= 32`   -> `GPIO.in1` (`GPIO::regs().in1()`)
//!
//! matching how esp-hal itself samples an input level (`esp-hal-1.1/src/gpio/mod.rs:554`,
//! `is_input_high` at `:1971`): `bank_register.read().bits() & (1 << (pin % 32)) != 0`.
//!
//! ## Full pin-type parity (task 2rw)
//! [`read_pin`] dispatches on [`PinType`]:
//!  * **Digital** — read the GPIO input register (non-invasive, below).
//!  * **PWM** — read the registered LEDC channel's live duty register (non-invasive, like the
//!    digital read: [`ledc_duty`] snapshots the duty-readback register without owning an
//!    esp-hal `Ledc` channel), then scale via [`PinReading::pwm`] (`mapLedcReadTo8Bit`,
//!    `gpio_viewer.h:1062`).
//!  * **Analog** — the ESP32 ADC is *not* register-readable non-invasively (a one-shot
//!    conversion needs a live `Adc` driver + configured `AdcPin`), so the reading is supplied
//!    by the firmware-injected [`crate::AnalogSource`] threaded in as a `fn` pointer. Without
//!    an injected source the arm returns `analog(0)` — see [`crate::AnalogSource`].
//!
//! ## Host / no-chip builds
//! The actual register access is gated behind the chip features (`esp32` / `esp32s3`),
//! because `esp-hal` only compiles with a chip selected. Under `--features server` with no
//! chip (the host clippy build), [`digital_level`] and [`ledc_duty`] compile to hardware-free
//! stubs (level low / duty 0), so the sampler and SSE handler still type-check on the host.

use crate::value::PinReading;
use crate::{AnalogSource, PinType, RegisteredPin};

/// Highest usable GPIO number + 1 for the selected chip, used to guard register reads.
///
/// ESP32 exposes GPIO `0..=39`; ESP32-S3 exposes `0..=48` (`gpio_viewer.h` sizes
/// `lastPinStates` by the analogous `maxGPIOPins`). The no-chip fallback keeps the sampler
/// compiling on the host and never reads hardware.
#[cfg(feature = "esp32")]
pub const MAX_GPIO_PINS: u8 = 40;
/// See [`MAX_GPIO_PINS`].
#[cfg(feature = "esp32s3")]
pub const MAX_GPIO_PINS: u8 = 49;
/// See [`MAX_GPIO_PINS`]. Host / no-chip fallback (no hardware present).
#[cfg(not(any(feature = "esp32", feature = "esp32s3")))]
pub const MAX_GPIO_PINS: u8 = 49;

/// Read one registered pin into a [`PinReading`], mirroring `readGPIO`
/// (`gpio_viewer.h:1010`).
///
/// `analog_source` is the firmware-injected ADC reader (from
/// [`crate::GpioViewer::analog_source`]); it is a `Copy` `fn` pointer threaded through so the
/// analog arm can read without this crate owning the `Adc`. It is ignored for digital and PWM
/// pins, which read hardware registers directly and non-invasively.
pub fn read_pin(registered: &RegisteredPin, analog_source: Option<AnalogSource>) -> PinReading {
    match registered.pin_type {
        // Digital arm — GPIO input register (`readGPIO` digital branch, `gpio_viewer.h:1051`).
        PinType::Digital => PinReading::digital(digital_level(registered.pin)),
        // PWM arm — live LEDC duty for the registered channel, scaled to 8 bits against
        // max_duty = (1 << resolution_bits) - 1 (`mapLedcReadTo8Bit`, `gpio_viewer.h:1062`).
        PinType::Pwm => {
            // A PWM pin missing its resolution yields max_duty 0 -> scaled 0 (safe); missing
            // its channel yields duty 0. Both degrade gracefully rather than panicking.
            let resolution_bits = registered.pwm_resolution_bits.unwrap_or(0);
            let max_duty = if resolution_bits == 0 {
                0
            } else {
                // LEDC resolution is <= 20 bits on real hardware, but a caller could pass a
                // bogus value; `checked_shl` guards against a debug-build shift-overflow panic
                // for `bits >= 32`, saturating max_duty to u32::MAX instead.
                1u32.checked_shl(resolution_bits as u32)
                    .unwrap_or(0)
                    .wrapping_sub(1)
            };
            let duty = match registered.pwm_channel {
                Some(channel) => ledc_duty(channel),
                None => 0,
            };
            PinReading::pwm(duty, max_duty)
        }
        // Analog arm — firmware-injected ADC reader (`analogRead`, `gpio_viewer.h:1040`); no
        // source injected -> raw 0 (frame shape stays correct, value static).
        PinType::Analog => {
            let raw = match analog_source {
                Some(read_adc) => read_adc(registered.pin) as u32,
                None => 0,
            };
            PinReading::analog(raw)
        }
    }
}

/// Sample a single pin's digital input level directly from the GPIO input registers.
///
/// Reproduces esp-hal's own `is_input_high` (`gpio/mod.rs:1971`): select the bank register
/// by `pin / 32`, then test bit `pin % 32`. Reading the register is non-invasive — it does
/// not require owning the pin, which is exactly what lets the viewer watch the app's pins.
#[cfg(any(feature = "esp32", feature = "esp32s3"))]
fn digital_level(pin: u8) -> bool {
    use esp_hal::peripherals::GPIO;

    // Out-of-range pins for the chip report low rather than indexing a nonexistent bit.
    if pin >= MAX_GPIO_PINS {
        return false;
    }

    // `GPIO::regs()` is esp-hal's `unstable` register-block accessor (enabled by our chip
    // features). `in_()` / `in1()` are the input registers; `.read().bits()` yields the
    // raw 32-bit snapshot for that bank.
    let registers = GPIO::regs();
    if pin < 32 {
        registers.in_().read().bits() & (1u32 << pin) != 0
    } else {
        // Bank 1 holds pins 32.., bit-indexed from 0 (hence `pin - 32`).
        registers.in1().read().bits() & (1u32 << (pin - 32)) != 0
    }
}

/// Host / no-chip stub: no GPIO hardware exists off-target, so every pin reads low. This
/// keeps [`read_pin`], the sampler, and the SSE handler compiling under `--features server`
/// without a chip selected (the host clippy build).
#[cfg(not(any(feature = "esp32", feature = "esp32s3")))]
fn digital_level(_pin: u8) -> bool {
    false
}

/// Snapshot the live duty of a LEDC channel from its duty-readback register (ESP32).
///
/// Non-invasive, exactly like [`digital_level`]: it reads the PAC register directly rather
/// than owning an esp-hal `Ledc` channel, so the viewer can observe a PWM output the
/// application drives. The ESP32 LEDC has two channel groups — high-speed channels `0..=7`
/// (`LEDC.hsch(n)`) and low-speed channels `8..=15` (`LEDC.lsch(n-8)`); channels outside
/// `0..=15` read as `0`.
///
/// The hardware duty register stores the value left-shifted by 4 fractional bits (esp-hal's
/// `set_duty_hw` writes `duty << 4`, `esp-hal-1.1/src/ledc/channel.rs:614`), so the logical
/// duty is the readback right-shifted by 4. `DUTY_R` reflects the *current* output duty
/// (`esp32` PAC `ledc::hsch::duty_r`), which is what a viewer wants to display.
#[cfg(feature = "esp32")]
fn ledc_duty(channel: u8) -> u32 {
    use esp_hal::peripherals::LEDC;

    // Lower 4 bits of the duty registers are fractional; shift them off to recover the
    // integer duty that maps against max_duty.
    const DUTY_FRACTIONAL_BITS: u32 = 4;

    let ledc = LEDC::regs();
    let raw = if channel < 8 {
        // High-speed group: channels 0..=7.
        ledc.hsch(channel as usize).duty_r().read().duty_r().bits()
    } else if channel < 16 {
        // Low-speed group: channels 8..=15 map to lsch indices 0..=7.
        ledc.lsch((channel - 8) as usize)
            .duty_r()
            .read()
            .duty_r()
            .bits()
    } else {
        // Out-of-range channel: report 0 rather than indexing a nonexistent channel.
        return 0;
    };
    raw >> DUTY_FRACTIONAL_BITS
}

/// Snapshot the live duty of a LEDC channel from its duty-readback register (ESP32-S3).
///
/// The ESP32-S3 LEDC has a single (low-speed) group of 8 channels `0..=7`, addressed by
/// `LEDC.ch(n)`; channels outside `0..=7` read as `0`. Same `>> 4` fixed-point convention as
/// the ESP32 path (see the `esp32` [`ledc_duty`]).
#[cfg(feature = "esp32s3")]
fn ledc_duty(channel: u8) -> u32 {
    use esp_hal::peripherals::LEDC;

    const DUTY_FRACTIONAL_BITS: u32 = 4;

    // ESP32-S3 exposes 8 channels only; anything higher has no register.
    if channel >= 8 {
        return 0;
    }
    let ledc = LEDC::regs();
    let raw = ledc.ch(channel as usize).duty_r().read().duty_r().bits();
    raw >> DUTY_FRACTIONAL_BITS
}

/// Host / no-chip stub: no LEDC hardware exists off-target, so every channel reads duty `0`.
/// Keeps [`read_pin`]'s PWM arm compiling under `--features server` without a chip selected.
#[cfg(not(any(feature = "esp32", feature = "esp32s3")))]
fn ledc_duty(_channel: u8) -> u32 {
    0
}
