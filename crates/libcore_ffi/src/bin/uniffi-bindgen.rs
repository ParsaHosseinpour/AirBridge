//! Bindings generator entry point.
//!
//! Build and run with the `cli` feature:
//!
//! ```text
//! cargo run -p libcore_ffi --features cli --bin uniffi-bindgen -- \
//!     generate --library target/release/core_ffi.dll --language kotlin --out-dir out
//! ```
fn main() {
    uniffi::uniffi_bindgen_main();
}
