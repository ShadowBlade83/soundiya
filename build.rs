fn main() {
    // Station firmwares ship an old ALSA whose unversioned buffer/period calls bind to the
    // obsolete ABI; controls/alsa_compat.c redirects them to the current entry points.
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("arm") {
        cc::Build::new()
            .file("controls/alsa_compat.c")
            .flag_if_supported("-std=c11")
            .flag_if_supported("-Os")
            .warnings(true)
            .warnings_into_errors(true)
            .compile("soundiya_alsa_compat");
        println!("cargo:rerun-if-changed=controls/alsa_compat.c");
        // Vendor ALSA plugins also load libraries from this directory.
        println!("cargo:rustc-link-arg=-Wl,--disable-new-dtags,-rpath,/system/vendor/quasar/libs");
        for name in [
            "get_buffer_size",
            "get_buffer_size_min",
            "get_buffer_size_max",
            "get_period_size",
            "get_period_size_min",
            "get_period_size_max",
            "set_buffer_size_near",
            "set_period_size_near",
        ] {
            println!("cargo:rustc-link-arg=-Wl,--wrap=snd_pcm_hw_params_{name}");
        }
    }
}
