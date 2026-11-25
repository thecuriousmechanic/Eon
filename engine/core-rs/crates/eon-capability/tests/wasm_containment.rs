#[cfg(target_os = "linux")]
mod approval_fixture;
use std::fs;
use std::io::Write;
use std::path::Path;

use eon_capability::{EonError, EonResult, wasm_tool::call_i32_export};
#[cfg(target_os = "linux")]
use eon_capability::{
    mcp_pinning::ToolDescriptor,
    registry::{AllowedTool, ToolInput},
};

#[cfg(target_os = "linux")]
fn assert_registry_rejects_imports(module: &[u8]) {
    let descriptor = ToolDescriptor {
        name: "fixture".into(),
        description: "local containment check".into(),
        input_schema: "{}".into(),
        annotations: "{}".into(),
    };
    // Even exact operator approval cannot grant an import.
    let mut registry = approval_fixture::prepared([AllowedTool {
        descriptor: descriptor.clone(),
        module_blake3: blake3::hash(module).into(),
    }])
    .unwrap();
    assert!(
        matches!(registry.register(descriptor, module), Err(EonError::SecurityViolation(m)) if m.contains("import"))
    );
    assert!(matches!(
        registry.dispatch(
            "fixture",
            &mut approval_fixture::workflow(),
            ToolInput { json: "0", label: &approval_fixture::operator() },
        ),
        Err(EonError::CapabilityNotFound { capability_id }) if capability_id == "fixture"
    ));
    let frames = eon_foundation::journal::replay(registry.root.with_file_name("journal")).unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[1].payload,
        b"tool.dispatch outcome attempt=1 not_registered"
    );
}

fn assert_missing_import(outcome: EonResult<i32>, namespace: &str, name: &str) {
    let Err(EonError::OperationFailed { operation, reason }) = outcome else {
        panic!("expected missing {namespace}::{name}, got {outcome:?}");
    };
    assert_eq!(operation, "wasm.instantiate");
    assert!(
        reason.contains(&format!("unknown import: `{namespace}::{name}`")),
        "{reason}"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn isolated_registry_worker_is_unavailable() {
    use eon_capability::wasm_tool::{MAX_WORKER_WALL_TIME, WasmWorker};
    assert!(WasmWorker::new(Path::new("/eon-wasm-worker"), MAX_WORKER_WALL_TIME).is_err());
}

#[test]
fn host_io_and_socket_creation_are_not_linked() {
    let fd_signature = "(param i32 i32 i32 i32) (result i32)";
    for (namespace, name, signature) in [
        ("wasi_snapshot_preview1", "fd_read", fd_signature),
        ("wasi_snapshot_preview1", "fd_write", fd_signature),
        (
            "wasi:sockets/tcp-create-socket@0.2.0",
            "create-tcp-socket",
            "(param i32 i32)",
        ),
    ] {
        // Even an unused import must be resolved before the guest can start.
        let module = format!(
            r#"(module (import "{namespace}" "{name}" (func {signature}))
            (memory (export "memory") 1) (func (export "run") (param i32) (result i32) local.get 0))"#
        );
        #[cfg(target_os = "linux")]
        assert_registry_rejects_imports(module.as_bytes());
        assert_missing_import(
            call_i32_export(module.as_bytes(), "run", 0, 1000),
            namespace,
            name,
        );
    }
}

#[test]
fn path_open_is_not_linked_and_the_host_sentinel_is_unchanged() {
    const CONTENTS: &[u8] = b"host-side fixture\n";
    let name = format!("wasm-sentinel-{:016x}", getrandom::u64().unwrap());
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    fs::File::create_new(&path)
        .unwrap()
        .write_all(CONTENTS)
        .unwrap();
    let bytes = path.as_os_str().as_encoded_bytes();
    let guest_path: String = bytes.iter().map(|b| format!(r"\{b:02x}")).collect();
    let module = format!(
        r#"(module
        (import "wasi_snapshot_preview1" "path_open" (func $open
            (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 16) "{guest_path}")
        (func (export "run") (param i32) (result i32)
            ;; dirfd, flags, path, length, TRUNC, FD_READ|FD_WRITE, inheriting rights, flags, result slot
            i32.const 3 i32.const 0 i32.const 16 i32.const {} i32.const 8
            i64.const 66 i64.const 0 i32.const 0 i32.const 0 call $open))"#,
        bytes.len()
    );
    #[cfg(target_os = "linux")]
    assert_registry_rejects_imports(module.as_bytes());
    let outcome = call_i32_export(module.as_bytes(), "run", 0, 1000);
    let observed = fs::read(&path);
    fs::remove_file(path).unwrap();
    assert_eq!(observed.unwrap(), CONTENTS);
    assert_missing_import(outcome, "wasi_snapshot_preview1", "path_open");
}
