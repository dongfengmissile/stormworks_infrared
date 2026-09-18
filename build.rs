//! Build variant selection by cargo profile:
//!
//! - `cargo build`           (debug profile)   -> verbose diagnostics build
//! - `cargo build --release` (release profile) -> player build, minimal logs
//!
//! Cargo has no per-profile feature selection, so the build script sets the
//! `logging` cfg from PROFILE. `--features logging` still forces the verbose
//! build in any profile.
//!
//! INFRARED_LOGGING=0 disables the cfg emission entirely, and
//! INFRARED_LOGGING=1 enables it, overriding the profile mapping.

fn main() {
    let forced = std::env::var("INFRARED_LOGGING").ok();

    let enable = match forced.as_deref() {
        Some("1") => true,
        Some("0") => false,
        _ => std::env::var("PROFILE").as_deref() == Ok("debug"),
    };

    if enable {
        println!("cargo::rustc-cfg=feature=\"logging\"");
    }
}
