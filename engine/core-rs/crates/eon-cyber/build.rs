fn main() {
    #[cfg(feature = "self-cpu-ebpf")]
    if matches!(
        std::env::var("CARGO_CFG_TARGET_OS").as_deref(),
        Ok("android" | "linux")
    ) {
        let source = "native/self_cpu.c";
        println!("cargo:rerun-if-changed={source}");
        cc::Build::new()
            .file(source)
            .flag("-fvisibility=hidden")
            .warnings_into_errors(true)
            .compile("eon_self_cpu");
    }
}
