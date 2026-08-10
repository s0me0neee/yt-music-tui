fn main() {
    // libmpv2-sys emits `cargo:rustc-link-lib=mpv` but no search path, so the
    // linker can't find Homebrew's libmpv (/opt/homebrew/lib). Probe via
    // pkg-config to emit the correct `rustc-link-search` portably (macOS/Linux).
    //
    // NOTE: libmpv2's own `build_libmpv` feature is NOT used here — it builds
    // mpv + ffmpeg from source via the mpv-build script at $MPV_SOURCE, which is
    // far heavier than linking the already-installed system libmpv.
    if let Err(e) = pkg_config::Config::new().probe("mpv") {
        println!("cargo:warning=pkg-config could not locate libmpv: {e}");
    }
}
