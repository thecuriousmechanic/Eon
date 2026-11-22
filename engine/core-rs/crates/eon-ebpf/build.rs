use std::{env, path::PathBuf, process::Command};

fn main() {
    let source = "bpf/self_fsync.bpf.c";
    println!("cargo:rerun-if-changed={source}");
    if env::var_os("CARGO_FEATURE_SELF_FSYNC").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }
    let target = match env::var("CARGO_CFG_TARGET_ENDIAN").as_deref() {
        Ok("little") => "bpfel",
        Ok("big") => "bpfeb",
        _ => panic!("unknown BPF target endianness"),
    };
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("self_fsync.o");
    let status = Command::new("clang")
        .args(["-target", target])
        .args(["-nostdinc", "-Wall", "-Werror", "-O2", "-g"])
        .args(["-fdebug-compilation-dir=.", "-c", source, "-o"])
        .arg(output)
        .status()
        .expect("self-fsync needs clang with a BPF backend");
    assert!(status.success(), "failed to compile the self-fsync sensor");
}
