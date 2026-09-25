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

    // Unique stamp per compiled binary, logged at bootstrap. On-disk checksums
    // cannot tell you what a RUNNING game has loaded; this can.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=INFRARED_BUILD_STAMP={stamp}");
}
