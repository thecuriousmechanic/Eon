#![cfg(target_os = "linux")]

mod approval_fixture;

use std::{fs, path::Path, time::Duration};

use approval_fixture::{operator, prepared, workflow};
use eon_capability::{
    EonError,
    ifc::{labels::CognitiveTaint, rule_of_two::Capability},
    mcp_pinning::ToolDescriptor,
    registry::{AllowedTool, ToolInput},
    wasm_tool::{DISPATCH_FUEL, MAX_OUTPUT_BYTES, MAX_WORKER_WALL_TIME, WasmWorker},
};
use eon_foundation::{
    journal::{HEADER_LEN, replay},
    scrubber::redaction_marker,
};

// Inputs beginning with "1" or "2" exceed fuel or output; "3" traps.
// Normal calls verify clean instance memory and the exact linear-memory cap.
const MODULE: &[u8] = br#"(module (memory (export "memory") 1)
  (func (export "run") (param $len i32) (result i32) (local $n i32)
    i32.const 1024 i32.load8_u if unreachable end
    i32.const 1024 i32.const 1 i32.store8
    i32.const 1 i32.load8_u i32.const 49 i32.eq if
      i32.const 100000 local.set $n
      (loop $count local.get $n i32.const 1 i32.sub local.tee $n br_if $count)
    end
    i32.const 1 i32.load8_u i32.const 50 i32.eq if i32.const -1 return end
    i32.const 1 i32.load8_u i32.const 51 i32.eq if unreachable end
    i32.const 1023 memory.grow i32.const 1 i32.ne if unreachable end
    i32.const 1 memory.grow i32.const -1 i32.ne if unreachable end
    local.get $len))"#;

fn assert_audit(dir: &Path, name: &str, statuses: &[&str], private: &[&str]) {
    let frames = replay(dir).unwrap();
    assert_eq!(frames.len(), 2 * statuses.len());
    let disk = fs::read(dir.join("seg_00000000000000000001.eonj")).unwrap();
    for raw in private {
        assert!(!disk.windows(raw.len()).any(|w| w == raw.as_bytes()));
    }

    let mut remaining = disk.as_slice();
    for (i, status) in statuses.iter().enumerate() {
        let attempt = 2 * i + 1;
        for (index, expected) in [
            format!("tool.dispatch attempt name={}", redaction_marker(name)),
            format!("tool.dispatch outcome attempt={attempt} {status}"),
        ]
        .iter()
        .enumerate()
        {
            let frame = &frames[2 * i + index];
            assert_eq!(frame.seq_no, (attempt + index) as u64);
            assert_eq!(frame.payload, expected.as_bytes());
            let (stored, rest) = remaining.split_at(HEADER_LEN + expected.len());
            assert_eq!(&stored[HEADER_LEN..], expected.as_bytes());
            remaining = rest;
        }
    }
    assert!(remaining.is_empty());
}

// This integration binary runs one test: the host descriptor count isn't
// racing unrelated tests. procfs children includes dead but unreaped workers.
#[test]
fn registered_dispatch_enforces_limits_reaps_workers_and_journals_only_redacted_metadata() {
    let name = ["ghp_", "paper-tool-name"].concat();
    let descriptor = ToolDescriptor {
        name: name.clone(),
        description: "local budget fixture".into(),
        input_schema: r#"{"type":"string"}"#.into(),
        annotations: "{}".into(),
    };
    let mut fixture = prepared([AllowedTool {
        descriptor: descriptor.clone(),
        module_blake3: blake3::hash(MODULE).into(),
    }])
    .unwrap();
    fixture.register(descriptor, MODULE).unwrap();

    let owner = operator();
    let input = |json| ToolInput {
        json,
        label: &owner,
    };
    let bearer = ["Bearer", "paper.payload.signature"].join(" ");
    let payload = format!("\"{bearer}; opaque-fixture\"");
    let payload = payload.as_str();
    let mut profile = workflow();
    let output = fixture
        .dispatch(&name, &mut profile, input(payload))
        .unwrap();
    assert_eq!(output.output, payload);
    assert_eq!(output.taint(), CognitiveTaint::UntrustedExternal);
    assert_eq!(profile.context, output.label);

    // Reuse the actual tainted ledger, not a separately manufactured label.
    profile.capabilities.push(Capability::MutateWorkspace);
    assert!(matches!(
        fixture.dispatch_guarded(&name, &mut profile, input(payload), Some(&output.output)),
        Err(EonError::SecurityViolation(m)) if m.contains("privileged")
    ));
    assert!(matches!(
        fixture.dispatch(&name, &mut workflow(), input("false")),
        Err(EonError::ValidationError(_))
    ));

    let open_fds = || fs::read_dir("/proc/self/fd").unwrap().count();
    let baseline = open_fds();
    let assert_released = || {
        assert!(
            fs::read_to_string("/proc/thread-self/children")
                .unwrap()
                .trim()
                .is_empty()
        );
        assert_eq!(open_fds(), baseline);
    };
    for (json, budget, requested, available) in [
        (r#""1""#, "wasm_fuel", DISPATCH_FUEL, 0),
        (
            r#""2""#,
            "tool_output_bytes",
            u64::from(u32::MAX),
            MAX_OUTPUT_BYTES as u64,
        ),
    ] {
        let failure = fixture.dispatch(&name, &mut workflow(), input(json));
        assert!(matches!(failure, Err(EonError::ResourceExhausted {
            resource, requested: used, available: limit
        }) if resource == budget && used == requested && limit == available));
        assert_released();
        // Same pin, worker and approval lock, no re-registration or reset.
        let next = fixture
            .dispatch(&name, &mut workflow(), input(payload))
            .unwrap();
        assert_eq!(next.output, payload);
    }

    assert!(matches!(
        fixture.dispatch_guarded(&name, &mut workflow(), input(r#""3""#), None),
        Err(EonError::OperationFailed { operation, .. }) if operation == "wasm.call"
    ));
    assert_released();

    // This budget can expire before spawn. The fuel case above, unlike this
    // one, requires an executed guest before checking resource reclamation.
    let worker = |time| WasmWorker::new(&fixture.executable, time).unwrap();
    let short = worker(Duration::from_nanos(1));
    let normal = worker(MAX_WORKER_WALL_TIME);
    fixture.registry = fixture.registry.with_wasm_worker(short);
    assert!(matches!(
        fixture.dispatch_guarded(&name, &mut workflow(), input(payload), None),
        Err(EonError::ResourceLeaseTimeout { lease_id }) if lease_id == "wasm.worker"
    ));
    fixture.registry = fixture.registry.with_wasm_worker(normal);
    assert_eq!(
        fixture
            .dispatch(&name, &mut workflow(), input(payload))
            .unwrap()
            .output,
        payload
    );

    let dir = fixture.root.with_file_name("journal");
    drop(fixture.registry);
    drop(fixture.journal); // stop the writer before independent disk readback
    assert_audit(
        &dir,
        &name,
        &[
            "ok",
            "denied",
            "invalid",
            "exhausted",
            "ok",
            "exhausted",
            "ok",
            "failed",
            "timeout",
            "ok",
        ],
        &[
            "paper-tool-name",
            "paper.payload.signature",
            "opaque-fixture",
        ],
    );
}
