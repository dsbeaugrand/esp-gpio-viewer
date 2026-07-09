//! Pin value/type mapping — the `readGPIO` semantics from `gpio_viewer.h:1010`.
//!
//! These helpers are pure and `no_std`-safe. They compute the `(s, v, t)` triple
//! the hosted UI expects for each pin, where `s` is the 0..=256 scaled value, `v`
//! is the raw reading, and `t` is the pin type discriminant.

use crate::PinType;

/// Arduino `map()` — integer, truncating toward zero, matching the semantics the
/// C++ library relies on at `gpio_viewer.h:786`, `:1046`, and `:1072`.
///
/// Formula: `(value - in_low) * (out_high - out_low) / (in_high - in_low) + out_low`.
///
/// Uses `i64` so intermediate products of ADC/PWM ranges cannot overflow. When the
/// input range is degenerate (`in_high == in_low`) we return `out_low` instead of
/// dividing by zero — the C++ code has no such guard, but a panic-free result is the
/// graceful choice for an embedded library.
pub fn map_value(value: i64, in_low: i64, in_high: i64, out_low: i64, out_high: i64) -> i64 {
    if in_high == in_low {
        return out_low;
    }
    (value - in_low) * (out_high - out_low) / (in_high - in_low) + out_low
}

/// A single pin's sampled state, ready to be serialized into a `gpio-state` frame.
///
/// Field names mirror the wire keys: `scaled` -> `s`, `raw` -> `v`, `pin_type` -> `t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinReading {
    /// Scaled 0..=256 value (`s` on the wire).
    pub scaled: u32,
    /// Raw reading (`v` on the wire): 0/1 for digital, duty for PWM, 0..=4095 for analog.
    pub raw: u32,
    /// Pin type discriminant (`t` on the wire).
    pub pin_type: PinType,
}

impl PinReading {
    /// Digital read (`gpio_viewer.h:1051-1059`): high -> `s=256, v=1`; low -> `s=0, v=0`.
    pub fn digital(high: bool) -> Self {
        if high {
            PinReading {
                scaled: 256,
                raw: 1,
                pin_type: PinType::Digital,
            }
        } else {
            PinReading {
                scaled: 0,
                raw: 0,
                pin_type: PinType::Digital,
            }
        }
    }

    /// PWM read (`mapLedcReadTo8Bit`, `gpio_viewer.h:1062-1072`): `v = duty`,
    /// `s = map(duty, 0, max_duty, 0, 255)`. A zero resolution / `max_duty` yields
    /// `s = 0`, matching the C++ early-return when `resolution == 0`.
    pub fn pwm(duty: u32, max_duty: u32) -> Self {
        let scaled = if max_duty == 0 {
            0
        } else {
            // clamp guards against a duty reading above max_duty producing s > 255.
            map_value(duty as i64, 0, max_duty as i64, 0, 255).clamp(0, 255) as u32
        };
        PinReading {
            scaled,
            raw: duty,
            pin_type: PinType::Pwm,
        }
    }

    /// Analog read (`gpio_viewer.h:1040-1047`): `v = raw` (0..=4095),
    /// `s = map(v, 0, 4095, 0, 255)` at the default 12-bit resolution.
    pub fn analog(raw: u32) -> Self {
        let scaled = map_value(raw as i64, 0, 4095, 0, 255).clamp(0, 255) as u32;
        PinReading {
            scaled,
            raw,
            pin_type: PinType::Analog,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_value_matches_arduino_midpoint() {
        // Arduino map(2048, 0, 4095, 0, 255) truncates to 127.
        assert_eq!(map_value(2048, 0, 4095, 0, 255), 127);
    }

    #[test]
    fn map_value_endpoints() {
        assert_eq!(map_value(0, 0, 4095, 0, 255), 0);
        assert_eq!(map_value(4095, 0, 4095, 0, 255), 255);
    }

    #[test]
    fn map_value_degenerate_range_returns_out_low() {
        // Guarded: no divide-by-zero panic when in_high == in_low.
        assert_eq!(map_value(5, 3, 3, 0, 255), 0);
    }

    #[test]
    fn digital_high_and_low() {
        assert_eq!(
            PinReading::digital(true),
            PinReading {
                scaled: 256,
                raw: 1,
                pin_type: PinType::Digital
            }
        );
        assert_eq!(
            PinReading::digital(false),
            PinReading {
                scaled: 0,
                raw: 0,
                pin_type: PinType::Digital
            }
        );
    }

    #[test]
    fn pwm_scales_duty() {
        // 8-bit resolution -> max_duty 255, duty 128 maps to 128.
        let reading = PinReading::pwm(128, 255);
        assert_eq!(reading.raw, 128);
        assert_eq!(reading.scaled, 128);
        assert_eq!(reading.pin_type, PinType::Pwm);
    }

    #[test]
    fn pwm_zero_max_duty_is_safe() {
        assert_eq!(PinReading::pwm(50, 0).scaled, 0);
    }

    #[test]
    fn analog_scales_raw() {
        let reading = PinReading::analog(2048);
        assert_eq!(reading.raw, 2048);
        assert_eq!(reading.scaled, 127);
        assert_eq!(reading.pin_type, PinType::Analog);
    }
}
