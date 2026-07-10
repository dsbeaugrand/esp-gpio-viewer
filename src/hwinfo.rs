//! Chip / flash / heap information for `GET /espinfo` (feature `server`).
//!
//! Builds a [`crate::protocol::EspInfo`] from **real esp-hal 1.0 reads** where a clean
//! `no_std` API exists, and **honest fallbacks** (`"n/a"` / `0` / `None`) where no such API
//! exists. This mirrors the C++ `sendESPInfo` (`gpio_viewer.h:353-495`), which leans on
//! Arduino-core/ESP-IDF helpers that have no direct Rust analogue.
//!
//! ## Field provenance
//! | Field                         | Source                                                              |
//! |-------------------------------|---------------------------------------------------------------------|
//! | `chip_model`, `cores_count`   | Compile-time constant selected by the active chip feature           |
//! | `chip_features`               | Compile-time constant per chip                                      |
//! | `cpu_frequency`               | `esp_hal::clock::Clocks::get().cpu_clock` (`gpio_viewer.h:355`)      |
//! | `chip_revision`               | `esp_hal::efuse::Efuse::chip_revision()` (`major*100 + minor`)       |
//! | `mac`                         | `esp_hal::efuse::Efuse::read_base_mac_address()` packed into a `u64` |
//! | `reset_reason(_code)`         | `esp_hal::rtc_cntl::reset_reason(Cpu::current())`                    |
//! | `up_time`, `uptime_us`        | `esp_hal::time::Instant::now().duration_since_epoch()`              |
//! | `free_heap`                   | Firmware-injected `free_heap_source` (allocator-agnostic)           |
//! | `free_psram`                  | Firmware-injected `free_psram_source` (allocator-agnostic)          |
//! | `flash_mode/size/speed`       | Honest `"n/a"`/`0` — no clean `no_std` esp-hal flash-info API        |
//! | `heap_size`, `heap_*`         | Honest `0` — total-heap/region stats need the allocator internals    |
//! | `cycle_count`                 | Honest `0` — no allocator-agnostic CCOUNT wrapper wired              |
//! | `sketch_size`, `free_sketch`  | Honest `0` — Arduino-IDE concepts with no esp-hal analogue           |
//! | `arduino_core_version`        | Honest `"n/a"` — not an Arduino build                               |
//! | `sdk_version`                 | `"esp-hal 1.0"` (the actual SDK backing this firmware)              |
//! | `idf_version`, `sketch_md5`   | Honest `None` — omitted (matches the C++ `#if`/length guards)        |
//! | `temperature_c`               | Honest `None` — the temperature sensor needs peripheral ownership    |
//!
//! Every read is feature-gated so the crate still type-checks under `--features server` with
//! no chip selected (the host/no-chip path returns the honest fallbacks above).

use crate::protocol;

/// The SDK backing this firmware. Reported verbatim as `sdk_version`; unlike the C++
/// `ESP.getSdkVersion()` (which returns an ESP-IDF string) this port is built on esp-hal.
const SDK_VERSION: &str = "esp-hal 1.0";

// --- Per-chip compile-time constants -----------------------------------------------------
// Exactly one chip module is active; the chip features are mutually exclusive at build time.

#[cfg(feature = "esp32")]
mod chip {
    /// `ESP.getChipModel()` equivalent, fixed by the selected chip feature.
    pub const MODEL: &str = "ESP32";
    /// `ESP.getChipCores()`.
    pub const CORES: u32 = 2;
    /// Radio capabilities advertised to the UI (`getChipFeatures`).
    pub const FEATURES: &[&str] = &["WiFi", "BLE", "BT"];
}

#[cfg(feature = "esp32s3")]
mod chip {
    pub const MODEL: &str = "ESP32-S3";
    pub const CORES: u32 = 2;
    pub const FEATURES: &[&str] = &["WiFi", "BLE"];
}

#[cfg(not(any(feature = "esp32", feature = "esp32s3")))]
mod chip {
    // Host / no-chip fallback: no chip selected, so nothing hardware-specific is known.
    pub const MODEL: &str = "unknown";
    pub const CORES: u32 = 0;
    pub const FEATURES: &[&str] = &[];
}

/// Live scalar readings collected from the chip in one shot.
struct Hardware {
    cpu_frequency: u32,
    chip_revision: u32,
    mac: u64,
    reset_code: i32,
    up_time: u32,
    uptime_us: u64,
}

/// Pack a 6-byte MAC (big-endian, as stored in eFuse) into the low 48 bits of a `u64`,
/// matching the single integer the C++ emits for `mac`.
#[cfg(any(feature = "esp32", feature = "esp32s3"))]
fn pack_mac(bytes: &[u8]) -> u64 {
    let mut mac: u64 = 0;
    for &byte in bytes.iter().take(6) {
        mac = (mac << 8) | byte as u64;
    }
    mac
}

/// Collect the live hardware readings (chip path).
#[cfg(any(feature = "esp32", feature = "esp32s3"))]
fn hardware() -> Hardware {
    // Chip revision as the ESP-IDF-style combined value `major*100 + minor`. In esp-hal 1.0
    // `Efuse::chip_revision()` returns that combined `u16` directly (in 1.1 it was a struct with
    // `.major`/`.minor`), so it is used as-is rather than recomputed.
    let revision = esp_hal::efuse::Efuse::chip_revision();
    // Base MAC programmed at manufacture (unstable esp-hal API, enabled by `unstable`). In 1.0
    // `Efuse::read_base_mac_address()` returns a plain `[u8; 6]` (no wrapper / `.as_bytes()`).
    let mac_bytes = esp_hal::efuse::Efuse::read_base_mac_address();
    let mac = pack_mac(&mac_bytes);
    // Reset reason for the current core; `None` maps to code 0 ("UNKNOWN").
    let reset_code = esp_hal::rtc_cntl::reset_reason(esp_hal::system::Cpu::current())
        .map(|reason| reason as i32)
        .unwrap_or(0);
    // Time since boot from the system timer.
    let elapsed = esp_hal::time::Instant::now().duration_since_epoch();

    Hardware {
        // esp-hal 1.0 exposes the configured CPU rate via `Clocks::get()` (no free `cpu_clock()`).
        cpu_frequency: esp_hal::clock::Clocks::get().cpu_clock.as_mhz(),
        chip_revision: revision as u32,
        mac,
        reset_code,
        up_time: elapsed.as_secs() as u32,
        uptime_us: elapsed.as_micros(),
    }
}

/// Host / no-chip fallback: honest zeros (the crate still type-checks without a chip).
#[cfg(not(any(feature = "esp32", feature = "esp32s3")))]
fn hardware() -> Hardware {
    Hardware {
        cpu_frequency: 0,
        chip_revision: 0,
        mac: 0,
        reset_code: 0,
        up_time: 0,
        uptime_us: 0,
    }
}

/// Map an esp-hal `SocResetReason` discriminant to an ESP-IDF-style reset string.
///
/// The match is on the numeric discriminant (not the chip-specific enum variant names), so a
/// single table serves every Xtensa chip. This is sound because `esp_hal::rtc_cntl::reset_reason`
/// builds the enum via `SocResetReason::from_repr(rtc_get_reset_reason(cpu))` — i.e. each
/// variant's discriminant **is** the raw RTC hardware reset code, so `reason as i32` yields that
/// raw code. Verified against `esp-hal` 1.0.0 `rtc_cntl/rtc/esp32.rs` / `esp32s3.rs`
/// (`ChipPowerOn = 0x01`, `CoreSw = 0x03`, `SysBrownOut = 0x0F`, …). Codes follow
/// `esp_reset_reason_t` (`gpio_viewer.h` prints `ESP.getResetReason()` at `:474`).
fn reset_reason_name(code: i32) -> &'static str {
    match code {
        0x01 => "POWERON_RESET",
        0x03 => "SW_RESET",
        0x05 => "DEEPSLEEP_RESET",
        0x06 => "SDIO_RESET",
        0x07 => "TG0WDT_SYS_RESET",
        0x08 => "TG1WDT_SYS_RESET",
        0x09 => "RTCWDT_SYS_RESET",
        0x0B => "TG0WDT_CPU_RESET",
        0x0C => "SW_CPU_RESET",
        0x0D => "RTCWDT_CPU_RESET",
        0x0E => "EXT_CPU_RESET",
        0x0F => "BROWNOUT_RESET",
        0x10 => "RTCWDT_RTC_RESET",
        0x11 => "TG1WDT_CPU_RESET",
        0x12 => "SUPER_WDT_RESET",
        0x13 => "CLK_GLITCH_RESET",
        0x14 => "EFUSE_RESET",
        _ => "UNKNOWN",
    }
}

/// Assemble the `/espinfo` payload from live chip reads plus the firmware-injected heap and
/// PSRAM figures.
///
/// `free_heap` and `free_psram` come from the allocator-agnostic injection seams
/// ([`crate::GpioViewer::free_heap_source`] / [`crate::GpioViewer::free_psram_source`]); every
/// other field is either a live esp-hal read or an honest fallback per the module table.
///
/// All returned string fields are `'static`, so the result borrows nothing and the caller can
/// hand it straight to [`protocol::espinfo_body`].
pub fn espinfo(free_heap: u32, free_psram: Option<u32>) -> protocol::EspInfo<'static> {
    let hardware = hardware();
    // We know *free* PSRAM (when a source is injected) but not the total, so `psram_size` and
    // `psram_max_alloc` stay honest zeros; `free_psram` carries the injected reading.
    let free_psram_bytes = free_psram.unwrap_or(0);

    protocol::EspInfo {
        chip_model: chip::MODEL,
        cores_count: chip::CORES,
        chip_revision: hardware.chip_revision,
        cpu_frequency: hardware.cpu_frequency,
        cycle_count: 0,
        mac: hardware.mac,
        flash_mode: "n/a",
        flash_chip_size: 0,
        flash_chip_speed: 0,
        heap_size: 0,
        heap_max_alloc: 0,
        psram_size: 0,
        free_psram: free_psram_bytes,
        psram_max_alloc: 0,
        free_heap,
        heap_free_8bit: 0,
        heap_free_32bit: 0,
        heap_largest_free_block: 0,
        up_time: hardware.up_time,
        uptime_us: hardware.uptime_us,
        sketch_size: 0,
        free_sketch: 0,
        arduino_core_version: "n/a",
        sdk_version: SDK_VERSION,
        idf_version: None,
        sketch_md5: None,
        chip_features: chip::FEATURES,
        reset_reason_code: hardware.reset_code,
        reset_reason: reset_reason_name(hardware.reset_code),
        temperature_c: None,
    }
}

/// Error from [`read_partition_infos`] when the flash partition table cannot be read or parsed.
#[cfg(any(feature = "esp32", feature = "esp32s3"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PartitionReadError {
    /// The partition table could not be read from flash or failed to parse.
    ReadFailed,
}

/// Read and map the on-flash ESP-IDF partition table into [`protocol::PartitionInfo`]s.
///
/// Enumerates the real partition table (`esp-bootloader-esp-idf`) and maps each entry to the
/// shape [`protocol::partition_body`] serializes, preserving the C++ **DATA-then-APP** ordering
/// (`gpio_viewer.h:318`). Generic over any [`embedded_storage::Storage`] so the crate does not
/// hard-depend on a specific flash driver: firmware passes its flash handle (e.g. an
/// `esp_storage::FlashStorage`) plus a scratch buffer (>= `PARTITION_TABLE_MAX_LEN`, `0xC00`).
///
/// `table_buffer` must outlive `out`: the emitted labels borrow the parsed table bytes, so a
/// caller that wants a `'static` slice (as [`crate::server::AppState::partitions`] requires)
/// keeps the buffer in a `'static` (e.g. a `StaticCell`). Returns the number of partitions
/// written, or `Err(())` if the table could not be read/parsed. Partitions beyond `N` are
/// dropped (graceful overflow, matching the rest of the crate).
///
/// Only compiled with a chip selected — `esp-bootloader-esp-idf` needs a chip feature.
#[cfg(any(feature = "esp32", feature = "esp32s3"))]
pub fn read_partition_infos<'buffer, Flash, const N: usize>(
    flash: &mut Flash,
    table_buffer: &'buffer mut [u8],
    out: &mut heapless::Vec<protocol::PartitionInfo<'buffer>, N>,
) -> Result<usize, PartitionReadError>
where
    Flash: embedded_storage::Storage,
{
    use esp_bootloader_esp_idf::partitions;

    out.clear();
    let table = partitions::read_partition_table(flash, table_buffer)
        .map_err(|_| PartitionReadError::ReadFailed)?;

    // The C++ lists DATA partitions before APP; emit in two ordered passes. `raw_type()` is
    // `0` for APP and `1` for DATA (`esp_partition_type_t`).
    for want_data in [true, false] {
        for entry in table.iter() {
            let is_data = entry.raw_type() == 1;
            if is_data != want_data {
                continue;
            }
            let info = protocol::PartitionInfo {
                label: entry.label_as_str(),
                ptype: entry.raw_type(),
                subtype: entry.raw_subtype(),
                address: entry.offset(),
                size: entry.len(),
            };
            // Capacity-bounded push: a full `out` silently drops extras (graceful overflow).
            if out.push(info).is_err() {
                break;
            }
        }
    }

    Ok(out.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn espinfo_carries_injected_heap_and_psram() {
        // On the host path the chip reads are 0, but the injected heap/psram must flow through.
        let info = espinfo(200_000, Some(4_194_304));
        assert_eq!(info.free_heap, 200_000);
        assert_eq!(info.free_psram, 4_194_304);
        // Total PSRAM is unknown without the allocator, so it stays an honest 0.
        assert_eq!(info.psram_size, 0);
    }

    #[test]
    fn espinfo_absent_psram_is_zero() {
        let info = espinfo(1_000, None);
        assert_eq!(info.free_psram, 0);
    }

    #[test]
    fn reset_reason_names_cover_common_codes() {
        assert_eq!(reset_reason_name(0x01), "POWERON_RESET");
        assert_eq!(reset_reason_name(0x0C), "SW_CPU_RESET");
        assert_eq!(reset_reason_name(0x0F), "BROWNOUT_RESET");
        assert_eq!(reset_reason_name(0x99), "UNKNOWN");
    }
}
