fn main() {
    #[cfg(feature = "sherpa-onnx")]
    {
        use sha2::{Digest, Sha256};
        use std::{env, fs, path::PathBuf};
        println!("cargo:rerun-if-env-changed=SHERPA_ONNX_DIR");
        let sdk = PathBuf::from(env::var_os("SHERPA_ONNX_DIR").expect("set SHERPA_ONNX_DIR to the v1.12.20 SDK"));
        let include = sdk.join("include");
        let header = include.join("sherpa-onnx/c-api/c-api.h");
        println!("cargo:rerun-if-changed={}", header.display());
        let digest = Sha256::digest(fs::read(&header).expect("missing Sherpa C ABI header"));
        assert_eq!(
            format!("{digest:x}"),
            "b575df36a35db0b384a708b05df968846af8c215ae5e9f3b76d35f31620d0ea3",
            "unexpected Sherpa header; use the verified v1.12.20 SDK"
        );
        println!("cargo:rerun-if-changed=src/asr/sherpa_config.c");
        cc::Build::new()
            .include(include)
            .file("src/asr/sherpa_config.c")
            .compile("eon_sherpa_config");
        // On Linux the SDK has its own older, symbol-versioned libonnxruntime.so.
        // Never let ort-sys's -lonnxruntime resolve to that library.
        #[cfg(feature = "onnx-speech")]
        {
            println!("cargo:rerun-if-env-changed=ORT_LIB_PATH");
            let ort = PathBuf::from(env::var_os("ORT_LIB_PATH").expect("both ASR backends require ORT_LIB_PATH"));
            println!("cargo:rustc-link-search=native={}", ort.display());
        }
        println!("cargo:rustc-link-search=native={}", sdk.join("lib").display());
        println!("cargo:rustc-link-lib=dylib=sherpa-onnx-c-api");
    }
}
