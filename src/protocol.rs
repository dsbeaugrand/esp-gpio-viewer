//! Pure JSON / SSE serializers for the GPIOViewer backend protocol.
//!
//! Every function here reproduces — byte for byte — the strings emitted by the C++
//! reference library (`gpio_viewer/src/gpio_viewer.h`) so the remotely hosted Vue UI
//! keeps working unchanged. Each serializer formats into a fixed-capacity
//! `heapless::String`, so there is no heap allocation and the module is `no_std`-safe.
//!
//! Buffer sizes are deliberately generous (see each function). If an input exceeds a
//! buffer, `heapless` silently drops the overflowing bytes rather than panicking; the
//! documented capacities cover the realistic pin/partition counts of ESP32 / ESP32-S3.

use core::fmt::Write;

use heapless::String;

/// Reproduce the malformed `/free_psram` output from `gpio_viewer.h:511-514` verbatim.
///
/// The repo's C++ closes the JSON early (the initial literal ends with `"}`), then
/// appends `<formatted>"}`, yielding invalid JSON like `{"sampling": "100"}1.50 KB"}`
/// that the hosted Vue UI cannot `JSON.parse`. That is a copy-paste regression (the
/// line is byte-identical to `sendSamplingInterval` at `:499`), not intended behavior.
///
/// We emit the corrected, parseable form by default. Flip this to `true` only if you
/// ever need to byte-match the buggy header exactly.
pub const FREE_PSRAM_REPRO_BUG: bool = false;

/// The remotely hosted Vue UI base URL (`gpio_viewer.h:30`).
///
/// The index page emits `<base href='...'>` pointing here so every relative asset
/// (`GPIOViewerVue.js`, `assets/main.css`, `favicon.ico`) resolves against the hosted
/// bundle rather than the device. Keeping this stable is what lets the port ship no
/// front-end assets of its own.
pub const BASE_URL: &str =
    "https://thelastoutpostworkshop.github.io/microcontroller_devkit/gpio_viewer_1_5/";

/// `GET /` index page (`generateIndexHTML`, `gpio_viewer.h:643-661`).
///
/// Reproduces the C++ string byte-for-byte, including two deliberate quirks preserved
/// so the hosted UI keeps parsing it unchanged:
///  * `<base href ='...'>` — note the space between `href` and `=` (`:646`).
///  * `window.gpio_settings = { ... }` with spaces around `=` (`:652`) but none inside
///    the object literal (`:653-655`).
///
/// `ip` is the device's dotted-quad address, `port` the HTTP port, and `free_sketch_ram`
/// the pre-formatted free-sketch-RAM string the C++ stores in `freeRAM` (`:655`). All
/// three are injected verbatim into the inline `window.gpio_settings` object.
///
/// The 1024-byte buffer comfortably holds the ~520-byte static template plus the base
/// URL and the three short runtime values; oversized inputs are silently truncated by
/// `heapless` rather than panicking.
pub fn index_html(ip: &str, port: u16, free_sketch_ram: &str) -> String<1024> {
    let mut out = String::new();
    // gpio_viewer.h:645 — document head opener.
    let _ = out.push_str("<!DOCTYPE html><html lang='en'><head><meta charset='UTF-8'>");
    // gpio_viewer.h:646 — the `href ='` space quirk is intentional and reproduced.
    let _ = write!(out, "<base href ='{}'>", BASE_URL);
    // gpio_viewer.h:647 — icon, viewport, and title.
    let _ = out.push_str(
        "<link rel='icon' href='favicon.ico'>\
         <meta name='viewport' content='width=device-width, initial-scale=1.0'>\
         <title>GPIOViewer</title>",
    );
    // gpio_viewer.h:648 — hosted Vue bundle.
    let _ = out.push_str("<script type='module' crossorigin src='GPIOViewerVue.js'></script>");
    // gpio_viewer.h:649 — hosted stylesheet, then the app mount point.
    let _ = out.push_str(
        "<link rel='stylesheet' crossorigin href='assets/main.css'>\
         </head><body><div id='app'></div>",
    );
    // gpio_viewer.h:651-657 — inline runtime settings the Vue app reads on boot.
    let _ = write!(
        out,
        "<script>window.gpio_settings = {{ip:'{}',port:'{}',freeSketchRam:'{}'}};</script>",
        ip, port, free_sketch_ram
    );
    // gpio_viewer.h:659 — document close.
    let _ = out.push_str("</body></html>");
    out
}

/// `GET /release` (`gpio_viewer.h:505`) -> `{"release": "<ver>"}` (note the space after the colon).
pub fn release_body(release: &str) -> String<64> {
    let mut out = String::new();
    // C++: "{\"release\": \"" + release + "\"}"
    let _ = write!(out, "{{\"release\": \"{}\"}}", release);
    out
}

/// `GET /sampling` (`gpio_viewer.h:499`) -> `{"sampling": "<ms>"}`.
pub fn sampling_body(sampling_ms: u32) -> String<64> {
    let mut out = String::new();
    let _ = write!(out, "{{\"sampling\": \"{}\"}}", sampling_ms);
    out
}

/// `GET /free_psram` (`gpio_viewer.h:509-517`).
///
/// `psram = Some(bytes)` emits the `formatBytes` string; `None` emits `No PSRAM`.
/// Default (corrected) shape: `{"sampling": "<ms> <value>"}`.
/// With [`FREE_PSRAM_REPRO_BUG`] set, reproduces the malformed header output verbatim.
pub fn free_psram_body(sampling_ms: u32, psram: Option<u32>) -> String<64> {
    free_psram_body_variant(sampling_ms, psram, FREE_PSRAM_REPRO_BUG)
}

/// Shared implementation for [`free_psram_body`], with the malformed-reproduction
/// branch made an explicit parameter rather than reading the module const.
///
/// This exists so both output shapes stay directly assertable in tests regardless of
/// how [`FREE_PSRAM_REPRO_BUG`] is set — otherwise the disabled branch would be dead
/// code and "both branches tested" would only be nominally true.
fn free_psram_body_variant(sampling_ms: u32, psram: Option<u32>, repro_bug: bool) -> String<64> {
    // The "value" portion is either the formatted byte count or the literal "No PSRAM".
    let mut value: String<24> = String::new();
    match psram {
        Some(bytes) => {
            let _ = write!(value, "{}", format_bytes(bytes));
        }
        // gpio_viewer.h:518 appends the literal "No PSRAM" when psramFound() is false.
        None => {
            let _ = value.push_str("No PSRAM");
        }
    }

    let mut out = String::new();
    if repro_bug {
        // Verbatim malformed reproduction of gpio_viewer.h:511-514:
        // {"sampling": "<ms>"} immediately followed by <value>"}
        let _ = write!(out, "{{\"sampling\": \"{}\"}}{}\"}}", sampling_ms, value);
    } else {
        // Corrected, JSON-parseable form the hosted Vue UI can consume.
        let _ = write!(out, "{{\"sampling\": \"{} {}\"}}", sampling_ms, value);
    }
    out
}

/// `formatBytes` (`gpio_viewer.h:1114`):
/// `< 1024` -> `"<n> B"`; `< 1024*1024` -> `"<kb> KB"` (2 decimals);
/// else -> `"<mb> MB"` (2 decimals).
///
/// The C++ uses `String(x, 2)` (float, 2 decimals). We compute the two decimals with
/// integer math (ties rounded away from zero) so the output is deterministic and free
/// of floating-point formatting drift.
pub fn format_bytes(bytes: u32) -> String<24> {
    let mut out = String::new();
    if bytes < 1024 {
        let _ = write!(out, "{} B", bytes);
    } else if bytes < 1024 * 1024 {
        let (whole, hundredths) = to_two_decimals(bytes as u64, 1024);
        let _ = write!(out, "{}.{:02} KB", whole, hundredths);
    } else {
        let (whole, hundredths) = to_two_decimals(bytes as u64, 1024 * 1024);
        let _ = write!(out, "{}.{:02} MB", whole, hundredths);
    }
    out
}

/// Return `(whole, hundredths)` of `numerator / divisor` rounded to two decimals,
/// ties rounded away from zero (matching Arduino's `dtostrf` behavior for positives).
fn to_two_decimals(numerator: u64, divisor: u64) -> (u64, u64) {
    // Multiply by 100 first, add half a divisor for round-half-up, then divide.
    let hundredths = (numerator * 100 + divisor / 2) / divisor;
    (hundredths / 100, hundredths % 100)
}

/// `GET /pinmodes` (`gpio_viewer.h:342-345`) -> `[{"pin":"<n>","mode":"<m>"},...]`.
///
/// Both `pin` and `mode` are STRING-quoted integers, exactly as the C++ emits. An
/// empty registry produces `[]`. Capacity covers the full 48-pin registry.
pub fn pinmodes_body(pins: &[(u8, u32)]) -> String<2048> {
    let mut out = String::new();
    let _ = out.push('[');
    for (index, (pin, mode)) in pins.iter().enumerate() {
        if index > 0 {
            let _ = out.push(',');
        }
        let _ = write!(out, "{{\"pin\":\"{}\",\"mode\":\"{}\"}}", pin, mode);
    }
    let _ = out.push(']');
    out
}

/// `GET /pinfunctions` (`gpio_viewer.h:1131-1152`).
///
/// Shape:
/// `{"boardpinsfunction":[{"name":"ADC", "functions":[{"function":"ADC","pin":<n>},...]},{"name":"Touch", "functions":[...]}]}`
///
/// Note the deliberate spacing quirks: a space after the comma before `"functions"`
/// (`startPinFunction`, `:1159`) but NO spaces inside each function object
/// (`addPinFunction`, `:1169`).
pub fn pinfunctions_body(adc_pins: &[u8], touch_pins: &[u8]) -> String<2048> {
    let mut out = String::new();
    let _ = out.push_str("{\"boardpinsfunction\":[");
    append_pin_function_group(&mut out, "ADC", adc_pins);
    // gpio_viewer.h:1144 separates the two groups with a bare comma.
    let _ = out.push(',');
    append_pin_function_group(&mut out, "Touch", touch_pins);
    let _ = out.push_str("]}");
    out
}

/// Emit one `{"name":"<fn>", "functions":[...]}` group used by [`pinfunctions_body`].
fn append_pin_function_group<const CAP: usize>(out: &mut String<CAP>, name: &str, pins: &[u8]) {
    // startPinFunction (gpio_viewer.h:1159): space after the comma, before "functions".
    let _ = write!(out, "{{\"name\":\"{}\", \"functions\":[", name);
    for (index, pin) in pins.iter().enumerate() {
        if index > 0 {
            let _ = out.push(',');
        }
        // addPinFunction (gpio_viewer.h:1169): no spaces inside the object.
        let _ = write!(out, "{{\"function\":\"{}\",\"pin\":{}}}", name, pin);
    }
    // endPinFunction (gpio_viewer.h:1174).
    let _ = out.push_str("]}");
}

/// One flash-partition entry for [`partition_body`].
#[derive(Debug, Clone, Copy)]
pub struct PartitionInfo<'a> {
    /// Partition label.
    pub label: &'a str,
    /// `esp_partition_type_t` value.
    pub ptype: u8,
    /// `esp_partition_subtype_t` value.
    pub subtype: u8,
    /// Flash offset (rendered as lowercase hex, `0x`-prefixed).
    pub address: u32,
    /// Partition size in bytes.
    pub size: u32,
}

/// `GET /partition` (`gpio_viewer.h:308-314`) -> array of
/// `{"label":"..","type":<n>,"subtype":<n>,"address":"0x<hex>","size":<n>}`.
///
/// The C++ lists DATA partitions first, then APP; preserve caller-supplied order.
/// `address` is lowercase hex without zero padding (Arduino `String(x, HEX)`).
pub fn partition_body(partitions: &[PartitionInfo]) -> String<2048> {
    let mut out = String::new();
    let _ = out.push('[');
    for (index, partition) in partitions.iter().enumerate() {
        if index > 0 {
            let _ = out.push(',');
        }
        let _ = write!(
            out,
            "{{\"label\":\"{}\",\"type\":{},\"subtype\":{},\"address\":\"0x{:x}\",\"size\":{}}}",
            partition.label, partition.ptype, partition.subtype, partition.address, partition.size
        );
    }
    let _ = out.push(']');
    out
}

/// Values for the `GET /espinfo` object (`sendESPInfo`, `gpio_viewer.h:353-495`).
///
/// The keys and their quoting rules mirror the C++ `appendField`/`appendRawField`
/// calls exactly; this task supplies the correct KEY names and shape, not real
/// hardware readings. `idf_version`, `sketch_md5`, and `temperature_c` are optional
/// (the C++ only appends them under `#if`/length/`isnan` guards).
#[derive(Debug, Clone, Copy)]
pub struct EspInfo<'a> {
    pub chip_model: &'a str,
    pub cores_count: u32,
    pub chip_revision: u32,
    pub cpu_frequency: u32,
    pub cycle_count: u64,
    pub mac: u64,
    pub flash_mode: &'a str,
    pub flash_chip_size: u32,
    pub flash_chip_speed: u32,
    pub heap_size: u32,
    pub heap_max_alloc: u32,
    pub psram_size: u32,
    pub free_psram: u32,
    pub psram_max_alloc: u32,
    pub free_heap: u32,
    pub heap_free_8bit: u32,
    pub heap_free_32bit: u32,
    pub heap_largest_free_block: u32,
    pub up_time: u32,
    pub uptime_us: u64,
    pub sketch_size: u32,
    pub free_sketch: u32,
    pub arduino_core_version: &'a str,
    pub sdk_version: &'a str,
    pub idf_version: Option<&'a str>,
    pub sketch_md5: Option<&'a str>,
    /// Pre-split chip-feature names, rendered as a raw `["A","B"]` JSON array.
    pub chip_features: &'a [&'a str],
    pub reset_reason_code: i32,
    pub reset_reason: &'a str,
    /// Temperature in Celsius, rendered raw with two decimals when present.
    pub temperature_c: Option<f32>,
}

/// Append `"<key>":"<value>"` (quoted) to the espinfo buffer, prefixing a comma
/// unless this is the first field. Mirrors `appendField(..., true)`.
fn append_quoted<const CAP: usize, T: core::fmt::Display>(
    out: &mut String<CAP>,
    is_first: &mut bool,
    key: &str,
    value: T,
) {
    if !*is_first {
        let _ = out.push(',');
    }
    *is_first = false;
    let _ = write!(out, "\"{}\":\"{}\"", key, value);
}

/// Append `"<key>":<value>` (raw / unquoted). Mirrors `appendField(..., false)` and `appendRawField`.
fn append_raw<const CAP: usize, T: core::fmt::Display>(
    out: &mut String<CAP>,
    is_first: &mut bool,
    key: &str,
    value: T,
) {
    if !*is_first {
        let _ = out.push(',');
    }
    *is_first = false;
    let _ = write!(out, "\"{}\":{}", key, value);
}

/// `GET /espinfo` (`gpio_viewer.h:422-493`). Emits fields in the exact C++ order with
/// the exact quoted/raw treatment; optional fields are omitted when `None`/empty.
pub fn espinfo_body(info: &EspInfo) -> String<2048> {
    let mut out: String<2048> = String::new();
    let _ = out.push('{');
    let mut is_first = true;

    append_quoted(&mut out, &mut is_first, "chip_model", info.chip_model);
    append_quoted(&mut out, &mut is_first, "cores_count", info.cores_count);
    append_quoted(&mut out, &mut is_first, "chip_revision", info.chip_revision);
    append_quoted(&mut out, &mut is_first, "cpu_frequency", info.cpu_frequency);
    append_raw(&mut out, &mut is_first, "cycle_count", info.cycle_count);
    append_quoted(&mut out, &mut is_first, "mac", info.mac);
    append_quoted(&mut out, &mut is_first, "flash_mode", info.flash_mode);
    append_raw(
        &mut out,
        &mut is_first,
        "flash_chip_size",
        info.flash_chip_size,
    );
    append_raw(
        &mut out,
        &mut is_first,
        "flash_chip_speed",
        info.flash_chip_speed,
    );
    append_raw(&mut out, &mut is_first, "heap_size", info.heap_size);
    append_raw(
        &mut out,
        &mut is_first,
        "heap_max_alloc",
        info.heap_max_alloc,
    );
    append_raw(&mut out, &mut is_first, "psram_size", info.psram_size);
    append_raw(&mut out, &mut is_first, "free_psram", info.free_psram);
    append_raw(
        &mut out,
        &mut is_first,
        "psram_max_alloc",
        info.psram_max_alloc,
    );
    append_raw(&mut out, &mut is_first, "free_heap", info.free_heap);
    append_raw(
        &mut out,
        &mut is_first,
        "heap_free_8bit",
        info.heap_free_8bit,
    );
    append_raw(
        &mut out,
        &mut is_first,
        "heap_free_32bit",
        info.heap_free_32bit,
    );
    append_raw(
        &mut out,
        &mut is_first,
        "heap_largest_free_block",
        info.heap_largest_free_block,
    );
    append_quoted(&mut out, &mut is_first, "up_time", info.up_time);
    append_raw(&mut out, &mut is_first, "uptime_us", info.uptime_us);
    append_raw(&mut out, &mut is_first, "sketch_size", info.sketch_size);
    append_raw(&mut out, &mut is_first, "free_sketch", info.free_sketch);
    append_quoted(
        &mut out,
        &mut is_first,
        "arduino_core_version",
        info.arduino_core_version,
    );
    append_quoted(&mut out, &mut is_first, "sdk_version", info.sdk_version);
    // idf_version only under ESP_IDF_VERSION_MAJOR >= 4 (gpio_viewer.h:478-480).
    if let Some(idf_version) = info.idf_version {
        append_quoted(&mut out, &mut is_first, "idf_version", idf_version);
    }
    // sketch_md5 only when non-empty (gpio_viewer.h:481-484).
    if let Some(sketch_md5) = info.sketch_md5 {
        if !sketch_md5.is_empty() {
            append_quoted(&mut out, &mut is_first, "sketch_md5", sketch_md5);
        }
    }
    // chip_features is a raw JSON array (appendRawField, gpio_viewer.h:485).
    if !is_first {
        let _ = out.push(',');
    }
    is_first = false;
    let _ = out.push_str("\"chip_features\":[");
    for (index, feature) in info.chip_features.iter().enumerate() {
        if index > 0 {
            let _ = out.push(',');
        }
        let _ = write!(out, "\"{}\"", feature);
    }
    let _ = out.push(']');

    append_raw(
        &mut out,
        &mut is_first,
        "reset_reason_code",
        info.reset_reason_code,
    );
    append_quoted(&mut out, &mut is_first, "reset_reason", info.reset_reason);
    // temperature_c only when not NaN (gpio_viewer.h:488-491), raw with two decimals.
    if let Some(temperature) = info.temperature_c {
        let mut formatted: String<16> = String::new();
        let _ = write!(formatted, "{:.2}", temperature);
        append_raw(&mut out, &mut is_first, "temperature_c", formatted.as_str());
    }

    let _ = out.push('}');
    out
}

/// SSE `gpio-state` payload (`checkGPIOValues`, `gpio_viewer.h:681-700`).
///
/// Emits only the changed pins as `{"<pin>": {"s": <s>, "v": <v>, "t": <t>}, ...}`,
/// entries joined by `", "` (note the space). An empty change set produces `{}`.
/// Each tuple is `(pin, scaled, raw, pin_type)`.
///
/// Capacity holds roughly the full ESP32-S3 GPIO range changing at once.
pub fn gpio_state_body(changes: &[(u8, u32, u32, u8)]) -> String<2048> {
    let mut out = String::new();
    let _ = out.push('{');
    for (index, (pin, scaled, raw, pin_type)) in changes.iter().enumerate() {
        if index > 0 {
            // gpio_viewer.h:692 joins entries with ", " (comma + space).
            let _ = out.push_str(", ");
        }
        // gpio_viewer.h:694: "<pin>": {"s": <s>, "v": <v>, "t": <t>}
        let _ = write!(
            out,
            "\"{}\": {{\"s\": {}, \"v\": {}, \"t\": {}}}",
            pin, scaled, raw, pin_type
        );
    }
    let _ = out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_matches_reference_header() {
        // gpio_viewer.h:30 — the hosted bundle URL must stay byte-identical.
        assert_eq!(
            BASE_URL,
            "https://thelastoutpostworkshop.github.io/microcontroller_devkit/gpio_viewer_1_5/"
        );
    }

    #[test]
    fn index_html_exact_bytes() {
        // Full byte-for-byte reproduction of generateIndexHTML (gpio_viewer.h:643-661)
        // for a representative ip / port / freeRAM triple.
        let html = index_html("192.168.1.50", 8080, "1.20 MB");
        assert_eq!(
            html.as_str(),
            "<!DOCTYPE html><html lang='en'><head><meta charset='UTF-8'>\
             <base href ='https://thelastoutpostworkshop.github.io/microcontroller_devkit/gpio_viewer_1_5/'>\
             <link rel='icon' href='favicon.ico'>\
             <meta name='viewport' content='width=device-width, initial-scale=1.0'>\
             <title>GPIOViewer</title>\
             <script type='module' crossorigin src='GPIOViewerVue.js'></script>\
             <link rel='stylesheet' crossorigin href='assets/main.css'>\
             </head><body><div id='app'></div>\
             <script>window.gpio_settings = {ip:'192.168.1.50',port:'8080',freeSketchRam:'1.20 MB'};</script>\
             </body></html>"
        );
    }

    #[test]
    fn index_html_preserves_base_href_space_quirk() {
        // The `href ='` space (gpio_viewer.h:646) is a reproduced quirk, not a typo.
        let html = index_html("10.0.0.1", 80, "0 B");
        assert!(html.as_str().contains("<base href ='"));
        // The base URL is emitted verbatim between the quirky opener and `'>`.
        assert!(html
            .as_str()
            .contains("<base href ='https://thelastoutpostworkshop.github.io/microcontroller_devkit/gpio_viewer_1_5/'>"));
    }

    #[test]
    fn index_html_injects_runtime_settings_verbatim() {
        // ip / port / freeSketchRam land inside window.gpio_settings unchanged,
        // with spaces around `=` but none inside the object literal.
        let html = index_html("172.16.5.9", 9090, "512 KB");
        assert!(html.as_str().contains(
            "window.gpio_settings = {ip:'172.16.5.9',port:'9090',freeSketchRam:'512 KB'};"
        ));
    }

    #[test]
    fn release_exact() {
        assert_eq!(release_body("1.7.1").as_str(), "{\"release\": \"1.7.1\"}");
    }

    #[test]
    fn sampling_exact() {
        assert_eq!(sampling_body(100).as_str(), "{\"sampling\": \"100\"}");
    }

    #[test]
    fn free_psram_some_clean_form() {
        // Corrected form: single field, space-separated value.
        assert_eq!(
            free_psram_body(100, Some(1536)).as_str(),
            "{\"sampling\": \"100 1.50 KB\"}"
        );
    }

    #[test]
    fn free_psram_none_clean_form() {
        assert_eq!(
            free_psram_body(100, None).as_str(),
            "{\"sampling\": \"100 No PSRAM\"}"
        );
    }

    #[test]
    fn format_bytes_boundaries() {
        assert_eq!(format_bytes(0).as_str(), "0 B");
        assert_eq!(format_bytes(512).as_str(), "512 B");
        assert_eq!(format_bytes(1023).as_str(), "1023 B");
        assert_eq!(format_bytes(1024).as_str(), "1.00 KB");
        assert_eq!(format_bytes(1536).as_str(), "1.50 KB");
        assert_eq!(format_bytes(1_048_576).as_str(), "1.00 MB");
    }

    #[test]
    fn pinmodes_empty() {
        let pins: [(u8, u32); 0] = [];
        assert_eq!(pinmodes_body(&pins).as_str(), "[]");
    }

    #[test]
    fn pinmodes_multi_entry_ordering() {
        let pins = [(2u8, 3u32), (15u8, 5u32)];
        assert_eq!(
            pinmodes_body(&pins).as_str(),
            "[{\"pin\":\"2\",\"mode\":\"3\"},{\"pin\":\"15\",\"mode\":\"5\"}]"
        );
    }

    #[test]
    fn pinfunctions_empty_groups() {
        let adc: [u8; 0] = [];
        let touch: [u8; 0] = [];
        assert_eq!(
            pinfunctions_body(&adc, &touch).as_str(),
            "{\"boardpinsfunction\":[{\"name\":\"ADC\", \"functions\":[]},{\"name\":\"Touch\", \"functions\":[]}]}"
        );
    }

    #[test]
    fn pinfunctions_populated_spacing() {
        let adc = [34u8, 35u8];
        let touch = [4u8];
        assert_eq!(
            pinfunctions_body(&adc, &touch).as_str(),
            "{\"boardpinsfunction\":[\
             {\"name\":\"ADC\", \"functions\":[{\"function\":\"ADC\",\"pin\":34},{\"function\":\"ADC\",\"pin\":35}]},\
             {\"name\":\"Touch\", \"functions\":[{\"function\":\"Touch\",\"pin\":4}]}]}"
        );
    }

    #[test]
    fn partition_empty() {
        let partitions: [PartitionInfo; 0] = [];
        assert_eq!(partition_body(&partitions).as_str(), "[]");
    }

    #[test]
    fn partition_two_entries_lowercase_hex() {
        let partitions = [
            PartitionInfo {
                label: "nvs",
                ptype: 1,
                subtype: 2,
                address: 0x9000,
                size: 20480,
            },
            PartitionInfo {
                label: "app0",
                ptype: 0,
                subtype: 16,
                address: 0x10000,
                size: 1310720,
            },
        ];
        assert_eq!(
            partition_body(&partitions).as_str(),
            "[{\"label\":\"nvs\",\"type\":1,\"subtype\":2,\"address\":\"0x9000\",\"size\":20480},\
             {\"label\":\"app0\",\"type\":0,\"subtype\":16,\"address\":\"0x10000\",\"size\":1310720}]"
        );
    }

    #[test]
    fn gpio_state_empty() {
        let changes: [(u8, u32, u32, u8); 0] = [];
        assert_eq!(gpio_state_body(&changes).as_str(), "{}");
    }

    #[test]
    fn gpio_state_two_pins_exact_spacing() {
        // Pin 2 digital-high, pin 4 analog (raw 2048 -> scaled 127).
        let changes = [(2u8, 256u32, 1u32, 0u8), (4u8, 127u32, 2048u32, 2u8)];
        assert_eq!(
            gpio_state_body(&changes).as_str(),
            "{\"2\": {\"s\": 256, \"v\": 1, \"t\": 0}, \"4\": {\"s\": 127, \"v\": 2048, \"t\": 2}}"
        );
    }

    #[test]
    fn espinfo_exact_with_all_optionals() {
        let features = ["WIFI_BGN", "BLE"];
        let info = EspInfo {
            chip_model: "ESP32-S3",
            cores_count: 2,
            chip_revision: 0,
            cpu_frequency: 240,
            cycle_count: 123456,
            mac: 1122334455,
            flash_mode: "QIO",
            flash_chip_size: 8388608,
            flash_chip_speed: 80000000,
            heap_size: 327680,
            heap_max_alloc: 200000,
            psram_size: 8388608,
            free_psram: 8000000,
            psram_max_alloc: 4000000,
            free_heap: 250000,
            heap_free_8bit: 250000,
            heap_free_32bit: 260000,
            heap_largest_free_block: 180000,
            up_time: 5000,
            uptime_us: 5000000,
            sketch_size: 900000,
            free_sketch: 2000000,
            arduino_core_version: "3.0.0",
            sdk_version: "v5.1.4",
            idf_version: Some("v5.1.4"),
            sketch_md5: Some("abc123"),
            chip_features: &features,
            reset_reason_code: 1,
            reset_reason: "POWERON",
            temperature_c: Some(42.5),
        };
        assert_eq!(
            espinfo_body(&info).as_str(),
            "{\"chip_model\":\"ESP32-S3\",\"cores_count\":\"2\",\"chip_revision\":\"0\",\
             \"cpu_frequency\":\"240\",\"cycle_count\":123456,\"mac\":\"1122334455\",\
             \"flash_mode\":\"QIO\",\"flash_chip_size\":8388608,\"flash_chip_speed\":80000000,\
             \"heap_size\":327680,\"heap_max_alloc\":200000,\"psram_size\":8388608,\
             \"free_psram\":8000000,\"psram_max_alloc\":4000000,\"free_heap\":250000,\
             \"heap_free_8bit\":250000,\"heap_free_32bit\":260000,\"heap_largest_free_block\":180000,\
             \"up_time\":\"5000\",\"uptime_us\":5000000,\"sketch_size\":900000,\"free_sketch\":2000000,\
             \"arduino_core_version\":\"3.0.0\",\"sdk_version\":\"v5.1.4\",\"idf_version\":\"v5.1.4\",\
             \"sketch_md5\":\"abc123\",\"chip_features\":[\"WIFI_BGN\",\"BLE\"],\
             \"reset_reason_code\":1,\"reset_reason\":\"POWERON\",\"temperature_c\":42.50}"
        );
    }

    #[test]
    fn espinfo_omits_absent_optionals() {
        let features: [&str; 0] = [];
        let info = EspInfo {
            chip_model: "ESP32",
            cores_count: 2,
            chip_revision: 3,
            cpu_frequency: 160,
            cycle_count: 1,
            mac: 2,
            flash_mode: "DIO",
            flash_chip_size: 4194304,
            flash_chip_speed: 40000000,
            heap_size: 100,
            heap_max_alloc: 90,
            psram_size: 0,
            free_psram: 0,
            psram_max_alloc: 0,
            free_heap: 80,
            heap_free_8bit: 80,
            heap_free_32bit: 85,
            heap_largest_free_block: 70,
            up_time: 10,
            uptime_us: 10000,
            sketch_size: 500,
            free_sketch: 600,
            arduino_core_version: "3.0.0",
            sdk_version: "v5.1.4",
            idf_version: None,
            sketch_md5: None,
            chip_features: &features,
            reset_reason_code: 0,
            reset_reason: "UNKNOWN",
            temperature_c: None,
        };
        // No idf_version, sketch_md5, or temperature_c fields; empty chip_features array.
        assert_eq!(
            espinfo_body(&info).as_str(),
            "{\"chip_model\":\"ESP32\",\"cores_count\":\"2\",\"chip_revision\":\"3\",\
             \"cpu_frequency\":\"160\",\"cycle_count\":1,\"mac\":\"2\",\"flash_mode\":\"DIO\",\
             \"flash_chip_size\":4194304,\"flash_chip_speed\":40000000,\"heap_size\":100,\
             \"heap_max_alloc\":90,\"psram_size\":0,\"free_psram\":0,\"psram_max_alloc\":0,\
             \"free_heap\":80,\"heap_free_8bit\":80,\"heap_free_32bit\":85,\
             \"heap_largest_free_block\":70,\"up_time\":\"10\",\"uptime_us\":10000,\
             \"sketch_size\":500,\"free_sketch\":600,\"arduino_core_version\":\"3.0.0\",\
             \"sdk_version\":\"v5.1.4\",\"chip_features\":[],\"reset_reason_code\":0,\
             \"reset_reason\":\"UNKNOWN\"}"
        );
    }

    #[test]
    fn free_psram_repro_bug_reproduces_malformed_header_output() {
        // Assert the verbatim malformed bytes unconditionally via the parameterized
        // helper, so this coverage holds no matter how FREE_PSRAM_REPRO_BUG is set.
        // These are exactly what gpio_viewer.h:511-518 emits (invalid JSON).
        assert_eq!(
            free_psram_body_variant(100, Some(1536), true).as_str(),
            "{\"sampling\": \"100\"}1.50 KB\"}"
        );
        assert_eq!(
            free_psram_body_variant(100, None, true).as_str(),
            "{\"sampling\": \"100\"}No PSRAM\"}"
        );
    }

    #[test]
    fn free_psram_corrected_form_matches_default() {
        // The parameterized helper's corrected branch must equal the public default,
        // guaranteeing free_psram_body delegates to the intended (parseable) shape.
        assert_eq!(
            free_psram_body_variant(100, Some(1536), false).as_str(),
            free_psram_body(100, Some(1536)).as_str()
        );
        assert_eq!(
            free_psram_body_variant(100, None, false).as_str(),
            free_psram_body(100, None).as_str()
        );
    }
}
