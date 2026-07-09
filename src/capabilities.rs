//! Per-chip ADC- and Touch-capable GPIO lists for the `/pinfunctions` endpoint.
//!
//! The C++ reference discovers these at runtime by probing every pin
//! (`readADCPinsConfiguration`, `gpio_viewer.h:916`; `readTouchPinsConfiguration`, `:993`).
//! For a `no_std` Rust port they are **static per-chip facts**, so we encode them as `const`
//! arrays — pure data with no hardware access, which keeps them host-testable and lets the
//! `/pinfunctions` handler emit real capability data instead of a stub.
//!
//! ## Source of truth
//! The pin numbers are taken from the esp-hal 1.1 analog-function tables
//! (`esp-metadata-generated-0.4.0`, `for_each_analog_function!` for `esp32` / `esp32s3`),
//! which are generated from the Espressif SVDs and agree with the device datasheets:
//!  * ESP32 datasheet §2.2 (18 ADC-capable pins, 10 touch pins T0..T9).
//!  * ESP32-S3 datasheet §2.2 (20 ADC-capable pins GPIO1..GPIO20, 14 touch pins T1..T14).
//!
//! Both lists are in ascending GPIO order — the natural order the C++ probe loop produces
//! (it scans pins `0..maxGPIOPins` upward).

/// ESP32 ADC-capable GPIOs (ADC1 CH0..7 + ADC2 CH0..9), ascending.
///
/// From `esp-metadata-generated` `for_each_analog_function!(esp32)`: ADC2 covers GPIO
/// 0/2/4/12/13/14/15/25/26/27, ADC1 covers GPIO 32..=39.
pub const ESP32_ADC_PINS: &[u8] = &[
    0, 2, 4, 12, 13, 14, 15, 25, 26, 27, 32, 33, 34, 35, 36, 37, 38, 39,
];

/// ESP32 touch-capable GPIOs (T0..T9), ascending.
///
/// From `for_each_analog_function!(esp32)` `TOUCH0..TOUCH9`: GPIO 0/2/4/12/13/14/15/27/32/33.
pub const ESP32_TOUCH_PINS: &[u8] = &[0, 2, 4, 12, 13, 14, 15, 27, 32, 33];

/// ESP32-S3 ADC-capable GPIOs (ADC1 CH0..9 + ADC2 CH0..9), ascending.
///
/// From `for_each_analog_function!(esp32s3)`: ADC1 covers GPIO1..=10, ADC2 covers GPIO11..=20
/// — i.e. every GPIO in `1..=20`.
pub const ESP32S3_ADC_PINS: &[u8] = &[
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
];

/// ESP32-S3 touch-capable GPIOs (T1..T14), ascending.
///
/// From `for_each_analog_function!(esp32s3)` `TOUCH1..TOUCH14`: GPIO1..=14.
pub const ESP32S3_TOUCH_PINS: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14];

// The `ADC_PINS` / `TOUCH_PINS` aliases select the list for the chip actually being built,
// so `server::pinfunctions_handler` stays chip-agnostic. The no-chip host build (used by
// `cargo clippy --features server` without a chip feature) falls back to the ESP32 lists so
// the handler and its tests still have realistic data to serialize.

/// ADC-capable GPIOs for the selected chip. Feeds `/pinfunctions`.
#[cfg(feature = "esp32")]
pub const ADC_PINS: &[u8] = ESP32_ADC_PINS;
/// Touch-capable GPIOs for the selected chip. Feeds `/pinfunctions`.
#[cfg(feature = "esp32")]
pub const TOUCH_PINS: &[u8] = ESP32_TOUCH_PINS;

/// See [`ADC_PINS`].
#[cfg(feature = "esp32s3")]
pub const ADC_PINS: &[u8] = ESP32S3_ADC_PINS;
/// See [`TOUCH_PINS`].
#[cfg(feature = "esp32s3")]
pub const TOUCH_PINS: &[u8] = ESP32S3_TOUCH_PINS;

/// See [`ADC_PINS`]. Host / no-chip fallback (defaults to the ESP32 list).
#[cfg(not(any(feature = "esp32", feature = "esp32s3")))]
pub const ADC_PINS: &[u8] = ESP32_ADC_PINS;
/// See [`TOUCH_PINS`]. Host / no-chip fallback (defaults to the ESP32 list).
#[cfg(not(any(feature = "esp32", feature = "esp32s3")))]
pub const TOUCH_PINS: &[u8] = ESP32_TOUCH_PINS;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::pinfunctions_body;

    #[test]
    fn esp32_lists_have_expected_cardinality() {
        // ESP32: 18 ADC-capable pins, 10 touch pins (T0..T9) per the datasheet.
        assert_eq!(ESP32_ADC_PINS.len(), 18);
        assert_eq!(ESP32_TOUCH_PINS.len(), 10);
    }

    #[test]
    fn esp32s3_lists_have_expected_cardinality() {
        // ESP32-S3: 20 ADC-capable pins (GPIO1..20), 14 touch pins (T1..T14).
        assert_eq!(ESP32S3_ADC_PINS.len(), 20);
        assert_eq!(ESP32S3_TOUCH_PINS.len(), 14);
    }

    #[test]
    fn lists_are_ascending_and_deduplicated() {
        // Ascending order (the C++ probe order) with no duplicate GPIO numbers.
        for list in [
            ESP32_ADC_PINS,
            ESP32_TOUCH_PINS,
            ESP32S3_ADC_PINS,
            ESP32S3_TOUCH_PINS,
        ] {
            for window in list.windows(2) {
                assert!(window[0] < window[1], "list must be strictly ascending");
            }
        }
    }

    #[test]
    fn esp32_capabilities_serialize_through_pinfunctions_body() {
        // The pure const-array -> /pinfunctions body path, exercised on the host by passing
        // the ESP32 lists explicitly. This is the exact JSON the handler emits on an ESP32.
        let body = pinfunctions_body(ESP32_ADC_PINS, ESP32_TOUCH_PINS);
        assert_eq!(
            body.as_str(),
            "{\"boardpinsfunction\":[\
             {\"name\":\"ADC\", \"functions\":[\
             {\"function\":\"ADC\",\"pin\":0},{\"function\":\"ADC\",\"pin\":2},\
             {\"function\":\"ADC\",\"pin\":4},{\"function\":\"ADC\",\"pin\":12},\
             {\"function\":\"ADC\",\"pin\":13},{\"function\":\"ADC\",\"pin\":14},\
             {\"function\":\"ADC\",\"pin\":15},{\"function\":\"ADC\",\"pin\":25},\
             {\"function\":\"ADC\",\"pin\":26},{\"function\":\"ADC\",\"pin\":27},\
             {\"function\":\"ADC\",\"pin\":32},{\"function\":\"ADC\",\"pin\":33},\
             {\"function\":\"ADC\",\"pin\":34},{\"function\":\"ADC\",\"pin\":35},\
             {\"function\":\"ADC\",\"pin\":36},{\"function\":\"ADC\",\"pin\":37},\
             {\"function\":\"ADC\",\"pin\":38},{\"function\":\"ADC\",\"pin\":39}]},\
             {\"name\":\"Touch\", \"functions\":[\
             {\"function\":\"Touch\",\"pin\":0},{\"function\":\"Touch\",\"pin\":2},\
             {\"function\":\"Touch\",\"pin\":4},{\"function\":\"Touch\",\"pin\":12},\
             {\"function\":\"Touch\",\"pin\":13},{\"function\":\"Touch\",\"pin\":14},\
             {\"function\":\"Touch\",\"pin\":15},{\"function\":\"Touch\",\"pin\":27},\
             {\"function\":\"Touch\",\"pin\":32},{\"function\":\"Touch\",\"pin\":33}]}]}"
        );
    }
}
