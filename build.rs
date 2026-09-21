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
