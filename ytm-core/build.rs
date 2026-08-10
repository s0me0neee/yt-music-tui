fn main() {
    println!("cargo:rerun-if-env-changed=LIBMPV_DIR");

    // An explicit directory wins over probing, and on Windows it is the only
    // thing that works: there is no pkg-config, and the mpv *player* builds
    // ship no import library at all — libmpv comes from the separate mpv-dev
    // package, unpacked wherever the user put it.
    if let Ok(dir) = std::env::var("LIBMPV_DIR")
        && !dir.trim().is_empty()
    {
        println!("cargo:rustc-link-search=native={dir}");
        return;
    }

    // libmpv2-sys emits `cargo:rustc-link-lib=mpv` but no search path, so the
    // linker can't find Homebrew's libmpv (/opt/homebrew/lib). Probe via
    // pkg-config to emit the correct `rustc-link-search` portably (macOS/Linux).
    //
    // NOTE: libmpv2's own `build_libmpv` feature is NOT used here — it builds
    // mpv + ffmpeg from source via the mpv-build script at $MPV_SOURCE, which is
    // far heavier than linking the already-installed system libmpv.
    if let Err(e) = pkg_config::Config::new().probe("mpv") {
        println!("cargo:warning=pkg-config could not locate libmpv: {e}");
        println!(
            "cargo:warning=set LIBMPV_DIR to the directory holding the libmpv import library \
             (libmpv.dll.a / libmpv.so / libmpv.dylib)"
        );
    }
}
