//! Build script: supply the esp-hal linker script to the Xtensa example firmware.
//!
//! The per-chip examples (`examples/esp32.rs`, `examples/esp32s3.rs`) are real binaries and
//! must be linked with esp-hal's `linkall.x`, which places code/data into the correct ESP32
//! memory regions. Without it the esp-radio blobs' windowed longcalls overflow the 1 GB
//! relocation window and linking fails ("dangerous relocation: windowed longcall crosses 1GB
//! boundary"). `linkall.x` lives in esp-hal's build output, already on the link search path.
//!
//! This is scoped two ways so it never touches the host build:
//!  * gated on `target_arch = "xtensa"`, so a host `cargo test` (aarch64/x86_64) is unaffected;
//!  * emitted as `rustc-link-arg-examples`, so it applies only to example binaries — never to
//!    the library rlib (`--features esp32,server`) or the host test harness.

fn main() {
    // `CARGO_CFG_TARGET_ARCH` is the arch of the target being built. Only the Xtensa firmware
    // needs the esp linker script; host builds must not receive it.
    let is_xtensa = std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("xtensa");
    if is_xtensa {
        println!("cargo:rustc-link-arg-examples=-Tlinkall.x");
    }
}
